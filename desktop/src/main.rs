//! Enclosed Space Searching Machine: a filesystem indexing and
//! visualization desktop app, built on iced.
//!
//! One backend worker per scan drives the synchronous `findex-client` on a
//! dedicated thread. A generation-keyed subscription owns its bounded event
//! stream, so replacing the scan cancels the old worker and its BEAM sidecar.

mod anonymize;
mod app;
mod backend;
mod charts;
mod format;
mod theme;
mod view;

use std::env;
use std::path::{Path, PathBuf};
use std::time::Duration;

use findex_client::MountPolicy;
use iced::futures::{SinkExt, Stream, StreamExt};
use iced::{Element, Size, Subscription, Task, stream};

use app::{ScanModel, Tab};
use backend::{BackendEvent, BackendOptions, SIZE_FIELD};

const RECENT_LIMIT: usize = 256;
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

fn main() -> iced::Result {
    let cli = match parse_arguments(env::args().skip(1)) {
        Ok(cli) => cli,
        Err(error) => {
            eprintln!("essm: {error}");
            std::process::exit(1);
        }
    };
    if cli.help {
        print_help();
        return Ok(());
    }
    theme::set_dark(cli.dark || system_prefers_dark());
    anonymize::set_anonymized(cli.anonymize);

    iced::application(
        move || (DesktopApp::new(cli.clone()), Task::none()),
        update,
        view,
    )
    .title(|_state: &DesktopApp| "Enclosed Space Searching Machine".to_owned())
    .subscription(subscription)
    .theme(|_state: &DesktopApp| theme::theme())
    .window_size(Size::new(1360.0, 860.0))
    .run()
}

#[derive(Clone, Debug, Default)]
struct Cli {
    help: bool,
    root: Option<String>,
    project_root: Option<PathBuf>,
    fields: Vec<String>,
    concurrency: Option<u32>,
    cross_mounts: bool,
    dark: bool,
    anonymize: bool,
    autoshot: Option<PathBuf>,
    record: Option<PathBuf>,
}

fn parse_arguments(arguments: impl Iterator<Item = String>) -> Result<Cli, String> {
    let mut arguments = arguments.peekable();
    let mut cli = Cli::default();

    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "-h" | "--help" => cli.help = true,
            "--cross-mounts" => cli.cross_mounts = true,
            "--dark" => cli.dark = true,
            "--anonymize" => cli.anonymize = true,
            "--autoshot" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| "--autoshot requires a directory".to_owned())?;
                cli.autoshot = Some(PathBuf::from(value));
            }
            "--record" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| "--record requires a directory".to_owned())?;
                cli.record = Some(PathBuf::from(value));
            }
            "--workspace-root" | "--project-root" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| format!("{argument} requires a value"))?;
                cli.project_root = Some(PathBuf::from(value));
            }
            "-c" | "--concurrency" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| format!("{argument} requires a value"))?;
                let parsed = value
                    .parse::<u32>()
                    .ok()
                    .filter(|&value| value > 0)
                    .ok_or_else(|| format!("{argument} must be a positive integer"))?;
                cli.concurrency = Some(parsed);
            }
            "--field" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| "--field requires a value".to_owned())?;
                if value.is_empty() {
                    return Err("--field must not be empty".to_owned());
                }
                cli.fields.push(value);
            }
            value if value.starts_with('-') => return Err(format!("unknown option: {value}")),
            value => {
                if cli.root.replace(value.to_owned()).is_some() {
                    return Err("at most one ROOT directory is accepted".to_owned());
                }
            }
        }
    }

    Ok(cli)
}

fn print_help() {
    println!(
        "\
Enclosed Space Searching Machine \u{2014} a filesystem indexing and visualization tool

usage: essm [OPTIONS] [ROOT]

Without ROOT the app opens on the scan form. Options prefill the form:
      --workspace-root PATH
                           directory containing findex and rust_client
      --project-root PATH  alias for --workspace-root
  -c, --concurrency N      traversal workers (default: BEAM schedulers)
      --field NAME         retain another metadata field (repeatable)
      --cross-mounts       cross filesystem and automount boundaries
      --dark               force dark mode (default: system appearance)
      --anonymize          hide the username in rendered paths (for sharing)
      --autoshot DIR       debug: cycle every view, save PNGs, then exit
      --record DIR         capture the live scan as PNG frames, then exit
  -h, --help               show this help

Compile the backend first with `(cd rust_client/backend && mix compile)`."
    );
}

/// Sanitizes a user-entered root before it is passed out of the form:
/// trims, expands `~`, and resolves to the canonical absolute path —
/// removing `.`/`..` segments, duplicate separators, and symlink
/// indirection — so the backend, the header, and Finder reveals all
/// agree on one real location.
fn sanitized_root(input: &str) -> Result<String, String> {
    let expanded = expand_tilde(input.trim());
    if expanded.is_empty() {
        return Err("enter a directory to index".to_owned());
    }
    let canonical =
        std::fs::canonicalize(&expanded).map_err(|_| format!("{expanded} is not a directory"))?;
    if !canonical.is_dir() {
        return Err(format!("{expanded} is not a directory"));
    }
    canonical
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("{expanded} is not valid UTF-8"))
}

/// Expands a leading `~` or `~/…` to the user's home directory, as a
/// shell would; any other path is returned unchanged.
fn expand_tilde(path: &str) -> String {
    if path != "~" && !path.starts_with("~/") {
        return path.to_owned();
    }
    match env::var("HOME") {
        Ok(home) if !home.is_empty() => format!("{home}{}", &path[1..]),
        _ => path.to_owned(),
    }
}

/// Whether macOS is currently in dark appearance; the app starts in the
/// system's mode unless `--dark` overrides it.
fn system_prefers_dark() -> bool {
    std::process::Command::new("/usr/bin/defaults")
        .args(["read", "-g", "AppleInterfaceStyle"])
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).contains("Dark"))
        .unwrap_or(false)
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct ScanConfig {
    generation: u64,
    root: String,
    project_root: PathBuf,
    fields: Vec<String>,
    concurrency: Option<u32>,
    cross_mounts: bool,
}

impl ScanConfig {
    fn options(&self) -> BackendOptions {
        BackendOptions {
            project_root: self.project_root.clone(),
            root: PathBuf::from(&self.root),
            fields: self.fields.clone(),
            concurrency: self.concurrency,
            poll_interval: POLL_INTERVAL,
            progress_interval: PROGRESS_INTERVAL,
            mount_policy: if self.cross_mounts {
                MountPolicy::Cross
            } else {
                MountPolicy::StayOnFilesystem
            },
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct SetupForm {
    pub root: String,
    /// Extra metadata fields beyond the always-retained baseline.
    pub fields: Vec<String>,
    pub concurrency: Option<u32>,
    pub cross_mounts: bool,
    pub error: Option<String>,
}

/// Worker-count choice for the traversal dropdown.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkerChoice(pub Option<u32>);

pub const WORKER_CHOICES: [WorkerChoice; 8] = [
    WorkerChoice(None),
    WorkerChoice(Some(1)),
    WorkerChoice(Some(2)),
    WorkerChoice(Some(4)),
    WorkerChoice(Some(6)),
    WorkerChoice(Some(8)),
    WorkerChoice(Some(12)),
    WorkerChoice(Some(16)),
];

impl std::fmt::Display for WorkerChoice {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            None => formatter.write_str("auto (BEAM schedulers)"),
            Some(workers) => write!(formatter, "{workers}"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Screen {
    Setup,
    Running,
}

struct DesktopApp {
    screen: Screen,
    setup: SetupForm,
    model: ScanModel,
    scan: Option<ScanConfig>,
    generation: u64,
    project_root: PathBuf,
    autoshot: Option<Autoshot>,
    recording: Option<Recording>,
}

/// Live-scan capture for demo clips: one frame every tick from launch
/// until the scan reaches a terminal phase, then a short walk through
/// the remaining views, then exit. Frames are PNGs for external GIF or
/// video assembly. Driven by `--record DIR`.
struct Recording {
    directory: PathBuf,
    frame: usize,
    /// A capture is in flight; skip further ticks until it lands.
    capturing: bool,
    /// Frames stored after the scan reached a terminal phase.
    post_complete: u32,
}

/// Debug screenshot tour: visits every view in both modes, captures the
/// window through iced's own renderer (no OS permissions involved), and
/// exits. Driven by `--autoshot DIR`.
struct Autoshot {
    directory: PathBuf,
    step: usize,
    /// Ticks alternate between arranging a step and capturing it, giving
    /// the runtime one settle interval per view.
    armed: bool,
    warmup: u32,
}

/// (file stem, tab, dark mode, deep focus) for every stop of the tour.
/// Deep-focus stops first descend to the deepest largest-child leaf so
/// the capture exercises long breadcrumb trails; they come last so the
/// earlier stops keep the root focus.
const AUTOSHOT_STEPS: &[(&str, Tab, bool, bool)] = &[
    ("1-treemap-light", Tab::Treemap, false, false),
    ("2-sunburst-light", Tab::Sunburst, false, false),
    ("3-graph-light", Tab::Graph, false, false),
    ("4-diagnostics-light", Tab::Diagnostics, false, false),
    ("5-treemap-dark", Tab::Treemap, true, false),
    ("6-sunburst-dark", Tab::Sunburst, true, false),
    ("7-graph-dark", Tab::Graph, true, false),
    ("8-diagnostics-dark", Tab::Diagnostics, true, false),
    ("9-graph-deep-light", Tab::Graph, false, true),
];

impl DesktopApp {
    fn new(cli: Cli) -> Self {
        let project_root = cli
            .project_root
            .clone()
            .unwrap_or_else(default_project_root);
        let setup = SetupForm {
            root: cli.root.clone().unwrap_or_default(),
            fields: cli.fields.clone(),
            concurrency: cli.concurrency,
            cross_mounts: cli.cross_mounts,
            error: None,
        };
        let mut state = Self {
            screen: Screen::Setup,
            setup,
            model: ScanModel::new(RECENT_LIMIT),
            scan: None,
            generation: 0,
            project_root,
            autoshot: cli.autoshot.clone().map(|directory| Autoshot {
                directory,
                step: 0,
                armed: false,
                warmup: 3,
            }),
            recording: cli.record.clone().map(|directory| Recording {
                directory,
                frame: 0,
                capturing: false,
                post_complete: 0,
            }),
        };
        if cli.root.is_some() {
            state.start_scan();
        }
        state
    }

    fn start_scan(&mut self) {
        let root = match sanitized_root(&self.setup.root) {
            Ok(root) => root,
            Err(error) => {
                self.setup.error = Some(error);
                return;
            }
        };
        // Reflect the sanitized path back into the form so the user
        // sees exactly what was indexed.
        self.setup.root = root.clone();
        if let Err(error) = validate_project(&self.project_root) {
            self.setup.error = Some(error);
            return;
        }
        let mut fields = vec!["type".to_owned(), SIZE_FIELD.to_owned()];
        for field in &self.setup.fields {
            if !fields.iter().any(|known| known == field) {
                fields.push(field.clone());
            }
        }

        self.generation += 1;
        self.setup.error = None;
        self.model = ScanModel::new(RECENT_LIMIT);
        self.scan = Some(ScanConfig {
            generation: self.generation,
            root,
            project_root: self.project_root.clone(),
            fields,
            concurrency: self.setup.concurrency,
            cross_mounts: self.setup.cross_mounts,
        });
        self.screen = Screen::Running;
    }
}

#[derive(Clone, Debug)]
pub enum Message {
    // Setup form
    RootChanged(String),
    WorkersSelected(WorkerChoice),
    CrossMountsToggled(bool),
    StartScan,
    NewScan,
    ThemeToggled,
    // Backend
    Backend(Box<BackendEvent>),
    // Visualization interactions
    TabSelected(Tab),
    FocusDirectory(u32),
    NodeSelected(u32),
    /// Reveals a path in Finder.
    Reveal(String),
    FilterChanged(String),
    DismissNotice,
    AutoshotTick,
    ShotTaken(iced::window::Screenshot),
}

fn update(state: &mut DesktopApp, message: Message) -> iced::Task<Message> {
    match message {
        Message::AutoshotTick if state.recording.is_some() => return recording_tick(state),
        Message::AutoshotTick => return autoshot_tick(state),
        Message::ShotTaken(shot) if state.recording.is_some() => {
            return recording_store(state, &shot);
        }
        Message::ShotTaken(shot) => return autoshot_store(state, &shot),
        Message::RootChanged(value) => state.setup.root = value,
        Message::WorkersSelected(choice) => state.setup.concurrency = choice.0,
        Message::CrossMountsToggled(value) => state.setup.cross_mounts = value,
        Message::StartScan => state.start_scan(),
        Message::NewScan => {
            // Dropping the config ends the subscription; the worker notices
            // its channels closing, releases the index, and stops the BEAM.
            state.scan = None;
            state.screen = Screen::Setup;
        }
        Message::ThemeToggled => theme::set_dark(!theme::is_dark()),
        Message::Backend(event) => state.model.apply_event(*event),
        Message::TabSelected(tab) => state.model.tab = tab,
        Message::FocusDirectory(directory_id) => state.model.focus_directory(directory_id),
        Message::NodeSelected(directory_id) => state.model.selected = Some(directory_id),
        Message::Reveal(path) => reveal_in_finder(state, path),
        Message::FilterChanged(value) => state.model.set_filter(value),
        Message::DismissNotice => state.model.notice = None,
    }
    iced::Task::none()
}

fn autoshot_tick(state: &mut DesktopApp) -> iced::Task<Message> {
    let Some(plan) = &mut state.autoshot else {
        return iced::Task::none();
    };
    if plan.warmup > 0 {
        plan.warmup -= 1;
        return iced::Task::none();
    }
    let Some(&(_, tab, dark, deep)) = AUTOSHOT_STEPS.get(plan.step) else {
        return iced::exit();
    };
    if !plan.armed {
        // Arrange the step now; the next tick captures it once settled.
        plan.armed = true;
        theme::set_dark(dark);
        state.model.tab = tab;
        if deep {
            let mut cursor = state.model.focus;
            while let Some(&largest) = state.model.children_by_size(cursor).first() {
                cursor = largest;
            }
            state.model.focus_directory(cursor);
        }
        return iced::Task::none();
    }
    iced::window::latest()
        .and_then(iced::window::screenshot)
        .map(Message::ShotTaken)
}

fn recording_tick(state: &mut DesktopApp) -> iced::Task<Message> {
    let Some(recording) = &mut state.recording else {
        return iced::Task::none();
    };
    if recording.capturing {
        return iced::Task::none();
    }
    recording.capturing = true;
    iced::window::latest()
        .and_then(iced::window::screenshot)
        .map(Message::ShotTaken)
}

fn recording_store(state: &mut DesktopApp, shot: &iced::window::Screenshot) -> iced::Task<Message> {
    let Some(recording) = &mut state.recording else {
        return iced::Task::none();
    };
    recording.capturing = false;
    let path = recording
        .directory
        .join(format!("frame-{:04}.png", recording.frame));
    if let Err(error) = save_png(&path, shot) {
        eprintln!("essm: could not save {}: {error}", path.display());
    }
    recording.frame += 1;

    let terminal = matches!(
        state.model.phase.as_str(),
        "complete" | "incomplete" | "fatal" | "disconnected"
    );
    if !terminal {
        return iced::Task::none();
    }
    // Hold the finished treemap, then walk the other views, giving
    // each tab enough frames to be read in the assembled clip.
    recording.post_complete += 1;
    state.model.tab = match recording.post_complete {
        0..=7 => Tab::Treemap,
        8..=15 => Tab::Sunburst,
        16..=23 => Tab::Graph,
        24..=31 => Tab::Diagnostics,
        _ => return iced::exit(),
    };
    iced::Task::none()
}

fn autoshot_store(state: &mut DesktopApp, shot: &iced::window::Screenshot) -> iced::Task<Message> {
    let Some(plan) = &mut state.autoshot else {
        return iced::Task::none();
    };
    let Some(&(name, _, _, _)) = AUTOSHOT_STEPS.get(plan.step) else {
        return iced::exit();
    };
    let path = plan.directory.join(format!("{name}.png"));
    if let Err(error) = save_png(&path, shot) {
        eprintln!("essm: could not save {}: {error}", path.display());
    }
    plan.step += 1;
    plan.armed = false;
    if plan.step >= AUTOSHOT_STEPS.len() {
        return iced::exit();
    }
    iced::Task::none()
}

fn save_png(path: &Path, shot: &iced::window::Screenshot) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let file = std::fs::File::create(path).map_err(|error| error.to_string())?;
    let mut encoder = png::Encoder::new(
        std::io::BufWriter::new(file),
        shot.size.width,
        shot.size.height,
    );
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|error| error.to_string())?;
    writer
        .write_image_data(&shot.rgba)
        .map_err(|error| error.to_string())?;
    Ok(())
}

/// Selects the path in a Finder window; errors surface as a notice.
fn reveal_in_finder(state: &mut DesktopApp, path: String) {
    let result = std::process::Command::new("/usr/bin/open")
        .arg("-R")
        .arg(&path)
        .spawn();
    if let Err(error) = result {
        state.model.notice = Some(app::Notice {
            text: format!("could not reveal {path}: {error}"),
            level: app::NoticeLevel::Error,
        });
    }
}

fn view(state: &DesktopApp) -> Element<'_, Message> {
    match state.screen {
        Screen::Setup => view::setup(&state.setup),
        Screen::Running => view::running(&state.model),
    }
}

fn subscription(state: &DesktopApp) -> Subscription<Message> {
    let backend = match &state.scan {
        Some(config) => {
            Subscription::run_with(config.clone(), |config| backend_stream(config.clone()))
        }
        None => Subscription::none(),
    };
    if state.recording.is_some() {
        Subscription::batch([backend, Subscription::run(recording_ticks)])
    } else if state.autoshot.is_some() {
        Subscription::batch([backend, Subscription::run(autoshot_ticks)])
    } else {
        backend
    }
}

/// A plain 1.4s metronome for the screenshot tour, thread-driven so no
/// async timer runtime is required.
/// A faster metronome for live-scan recording (~2 fps).
fn recording_ticks() -> impl Stream<Item = Message> + Send {
    metronome(500)
}

fn autoshot_ticks() -> impl Stream<Item = Message> + Send {
    metronome(1_400)
}

fn metronome(interval_ms: u64) -> impl Stream<Item = Message> + Send {
    stream::channel(4, async move |mut output| {
        let (sender, mut ticks) = iced::futures::channel::mpsc::unbounded();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(interval_ms));
                if sender.unbounded_send(()).is_err() {
                    break;
                }
            }
        });
        while let Some(()) = ticks.next().await {
            if output.send(Message::AutoshotTick).await.is_err() {
                break;
            }
        }
    })
}

fn backend_stream(config: ScanConfig) -> impl Stream<Item = Message> + Send {
    stream::channel(4, async move |mut output| {
        let handle = backend::spawn(config.options());
        let mut events = handle.events;
        // The receiver closing is the cancellation signal. The worker then
        // releases the index and shuts down the sidecar before it exits.
        let _worker = handle.worker;

        while let Some(event) = events.next().await {
            if output
                .send(Message::Backend(Box::new(event)))
                .await
                .is_err()
            {
                break;
            }
        }
    })
}

fn default_project_root() -> PathBuf {
    let current = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if is_workspace_root(&current) {
        current
    } else if let Some(parent) = current.parent().filter(|parent| is_workspace_root(parent)) {
        parent.to_path_buf()
    } else {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap_or(Path::new("."))
            .to_path_buf()
    }
}

fn is_workspace_root(project_root: &Path) -> bool {
    project_root.join("findex/mix.exs").is_file()
        && project_root.join("rust_client/backend/mix.exs").is_file()
}

fn validate_project(project_root: &Path) -> Result<(), String> {
    if backend::packaged_backend_available() {
        return Ok(());
    }

    if !is_workspace_root(project_root) {
        return Err(format!(
            "{} is not the workspace root (expected findex/mix.exs and \
             rust_client/backend/mix.exs)",
            project_root.display()
        ));
    }

    let backend_build = project_root.join("rust_client/backend/_build/dev/lib");
    if !backend_build
        .join("findex_rust_backend/ebin/Elixir.FindexRust.Bridge.beam")
        .is_file()
        || !backend_build.join("findex/priv/findex_nif.so").is_file()
    {
        return Err(
            "the Rust backend is not compiled; run `(cd rust_client/backend && mix compile)`"
                .to_owned(),
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_prefill_options() {
        let cli = parse_arguments(
            ["-c", "8", "--field", "modified_at", "/tmp"]
                .into_iter()
                .map(str::to_owned),
        )
        .unwrap();

        assert_eq!(cli.root.as_deref(), Some("/tmp"));
        assert_eq!(cli.concurrency, Some(8));
        assert_eq!(cli.fields, ["modified_at"]);
    }

    #[test]
    fn rejects_second_root_and_unknown_flags() {
        let error = parse_arguments(["/one", "/two"].into_iter().map(str::to_owned))
            .err()
            .unwrap();
        assert!(error.contains("at most one ROOT"));

        let error = parse_arguments(["--bogus"].into_iter().map(str::to_owned))
            .err()
            .unwrap();
        assert!(error.contains("unknown option"));
    }

    #[test]
    fn tilde_expands_to_the_home_directory() {
        let home = std::env::var("HOME").expect("HOME is set in tests");
        assert_eq!(expand_tilde("~"), home);
        assert_eq!(expand_tilde("~/Documents"), format!("{home}/Documents"));
        assert_eq!(expand_tilde("/tmp/~x"), "/tmp/~x");
        assert_eq!(expand_tilde("~user/x"), "~user/x");
    }

    #[test]
    fn roots_are_sanitized_to_canonical_directories() {
        // Dot segments, duplicate separators, and symlinks (macOS /tmp
        // links to /private/tmp) all collapse to one canonical path.
        let canonical_tmp = sanitized_root("/tmp").expect("canonicalize /tmp");
        assert_eq!(
            sanitized_root("/tmp/../tmp//.").as_deref(),
            Ok(&*canonical_tmp)
        );
        assert_eq!(sanitized_root("  /tmp  ").as_deref(), Ok(&*canonical_tmp));
        assert!(std::path::Path::new(&canonical_tmp).is_absolute());

        let home = std::env::var("HOME").expect("HOME is set in tests");
        assert_eq!(sanitized_root("~"), sanitized_root(&home));

        assert!(sanitized_root("").unwrap_err().contains("enter"));
        assert!(
            sanitized_root("/definitely/not/real")
                .unwrap_err()
                .contains("not a directory")
        );
    }

    #[test]
    fn start_scan_validates_the_form() {
        let mut state = DesktopApp::new(Cli::default());
        state.start_scan();
        assert!(
            state
                .setup
                .error
                .as_deref()
                .is_some_and(|error| error.contains("enter"))
        );
        assert_eq!(state.screen, Screen::Setup);

        state.setup.root = "/definitely/not/a/real/path".to_owned();
        state.start_scan();
        assert!(
            state
                .setup
                .error
                .as_deref()
                .is_some_and(|error| error.contains("not a directory"))
        );

        // A valid root proceeds to project validation; the fields the
        // form selected join the always-retained baseline.
        state.setup.root = "/tmp".to_owned();
        state.setup.fields = vec!["modified_at".to_owned(), "type".to_owned()];
        state.start_scan();
        if let Some(config) = &state.scan {
            assert_eq!(config.fields, ["type", backend::SIZE_FIELD, "modified_at"]);
        } else {
            // The backend build may be absent in this environment; the
            // form must then stay on the setup screen with an error.
            assert!(state.setup.error.is_some());
            assert_eq!(state.screen, Screen::Setup);
        }
    }
}
