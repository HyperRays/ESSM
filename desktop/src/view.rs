//! Widget trees: the setup form, the visualization shell, and diagnostics.
//!
//! The design is visualization-first: three interchangeable pictures of
//! the same size tree (treemap, sunburst, node graph) share a focus, a
//! selection, and an inspector, while every textual list and counter
//! lives in the diagnostics view. Nothing requires folder-by-folder
//! clicking; anything selectable can be revealed in Finder instead.

use iced::widget::canvas::Canvas;
use iced::widget::{
    Space, button, checkbox, column, container, mouse_area, pick_list, progress_bar, row,
    scrollable, text, text_input,
};
use iced::{Alignment, Border, Color, Element, Font, Length, Theme, mouse};

use crate::anonymize;
use crate::app::{NoticeLevel, ScanModel, Tab};
use crate::charts::{
    AreaChart, Donut, GraphNode, Histogram, NodeGraph, Sparkline, Sunburst, SunburstSlice, Treemap,
    TreemapCell, mix,
};
use crate::format::{format_duration, human_bytes, human_count};
use crate::theme::{self, AMBER, DEEP_BLUE, SIGNAL_RED, SKY_BLUE, ui};
use crate::{Message, SetupForm};

/// Rows rendered per scrollable list; the state itself stays bounded.
const LIST_LIMIT: usize = 100;
/// Hue cycle for sibling branches in the sunburst and node graph.
const BRANCH_PALETTE: [Color; 5] = [
    SKY_BLUE,
    theme::TEAL_GREEN,
    AMBER,
    theme::BLOSSOM_PINK,
    DEEP_BLUE,
];

// ---------------------------------------------------------------- setup --

pub fn setup(form: &SetupForm) -> Element<'_, Message> {
    let header = column![
        text("Enclosed Space Searching Machine")
            .size(24)
            .color(SKY_BLUE),
    ]
    .align_x(Alignment::Center)
    .width(Length::Fill);

    let location = form_section(
        "LOCATION",
        column![
            text_input("e.g. /Users/you/Projects", &form.root)
                .on_input(Message::RootChanged)
                .on_submit(Message::StartScan)
                .padding(10)
                .font(Font::MONOSPACE)
                .width(Length::Fill),
            text("The directory whose tree will be indexed.")
                .size(12)
                .color(ui().label),
        ]
        .spacing(6)
        .into(),
    );

    let traversal = form_section(
        "TRAVERSAL",
        column![
            column![
                text("Workers").size(12).color(ui().label),
                pick_list(
                    &crate::WORKER_CHOICES[..],
                    Some(crate::WorkerChoice(form.concurrency)),
                    Message::WorkersSelected,
                )
                .text_size(13)
                .padding([7, 10])
                .width(Length::Fixed(230.0)),
            ]
            .spacing(5),
            checkbox(form.cross_mounts)
                .label("Cross filesystem and automount boundaries")
                .on_toggle(Message::CrossMountsToggled),
        ]
        .spacing(11)
        .into(),
    );

    let footer_status: Element<'_, Message> = match &form.error {
        Some(error) => text(error.clone()).size(13).color(SIGNAL_RED).into(),
        None => text(format!("{} workers", crate::WorkerChoice(form.concurrency)))
            .size(12)
            .color(ui().label)
            .into(),
    };
    let footer = row![
        footer_status,
        Space::new().width(Length::Fill),
        button(text("Start Indexing").size(15))
            .on_press(Message::StartScan)
            .padding([11, 30])
            .style(|theme_ref: &Theme, status| {
                let mut style = button::primary(theme_ref, status);
                style.border = Border {
                    radius: 8.0.into(),
                    ..Border::default()
                };
                style
            }),
    ]
    .spacing(12)
    .align_y(Alignment::Center);

    let card =
        container(column![location, form_divider(), traversal, form_divider(), footer].spacing(18))
            .style(theme::panel)
            .padding(28)
            .width(Length::Fill);

    container(
        column![
            row![Space::new().width(Length::Fill), theme_toggle()],
            header,
            card
        ]
        .spacing(20)
        .max_width(640)
        .align_x(Alignment::Center),
    )
    .center_x(Length::Fill)
    .center_y(Length::Fill)
    .padding(24)
    .into()
}

/// The light/dark switch, shown on the setup screen and the sidebar.
fn theme_toggle() -> Element<'static, Message> {
    let label = if theme::is_dark() {
        "\u{2600} Light Mode"
    } else {
        "\u{263e} Dark Mode"
    };
    button(text(label).size(13).color(ui().label))
        .on_press(Message::ThemeToggled)
        .padding([5, 12])
        .style(button::text)
        .into()
}

fn form_section<'a>(label: &'static str, content: Element<'a, Message>) -> Element<'a, Message> {
    column![section_label(label), content].spacing(8).into()
}

fn form_divider() -> Element<'static, Message> {
    iced::widget::rule::horizontal(1)
        .style(|_theme: &Theme| iced::widget::rule::Style {
            color: ui().panel_highlight,
            radius: 0.0.into(),
            fill_mode: iced::widget::rule::FillMode::Full,
            snap: true,
        })
        .into()
}

fn section_label(label: &'static str) -> Element<'static, Message> {
    text(label).size(11).color(ui().label).into()
}

// ---------------------------------------------------------------- shell --

pub fn running(app: &ScanModel) -> Element<'_, Message> {
    let body: Element<'_, Message> = match app.tab {
        Tab::Treemap | Tab::Sunburst | Tab::Graph => visualization(app),
        Tab::Diagnostics => diagnostics(app),
    };

    let mut main = column![header(app)].spacing(12);
    if let Some(notice) = &app.notice {
        let color = match notice.level {
            NoticeLevel::Error => SIGNAL_RED,
            NoticeLevel::Info => ui().link,
        };
        main = main.push(
            container(
                row![
                    text(&notice.text).color(color),
                    Space::new().width(Length::Fill),
                    button(text("Dismiss").size(13)).on_press(Message::DismissNotice),
                ]
                .spacing(12)
                .align_y(Alignment::Center),
            )
            .style(theme::panel)
            .padding(10)
            .width(Length::Fill),
        );
    }
    main = main.push(body);

    row![
        sidebar(app),
        container(main).padding(16).width(Length::Fill)
    ]
    .into()
}

fn sidebar(app: &ScanModel) -> Element<'_, Message> {
    let mut navigation = column![
        text("Enclosed Space").size(19).color(SKY_BLUE),
        text("Searching Machine").size(19).color(SKY_BLUE),
        Space::new().height(Length::Fixed(14.0)),
    ]
    .spacing(2);

    for tab in Tab::ALL {
        let active = app.tab == tab;
        let badge = match tab {
            Tab::Diagnostics if app.counters.directories_failed > 0 => {
                format!("  {}", human_count(app.counters.directories_failed))
            }
            _ => String::new(),
        };
        navigation = navigation.push(
            button(
                row![
                    text(tab.title()).size(15),
                    text(badge).size(13).color(SIGNAL_RED),
                ]
                .spacing(4),
            )
            .on_press(Message::TabSelected(tab))
            .width(Length::Fill)
            .padding([8, 14])
            .style(move |theme_ref: &Theme, status| {
                let mut style = button::text(theme_ref, status);
                if active {
                    style.background = Some(iced::Background::Color(ui().panel_highlight));
                    style.text_color = ui().link;
                    style.border = Border {
                        radius: 8.0.into(),
                        ..Border::default()
                    };
                }
                style
            }),
        );
    }

    navigation = navigation.push(Space::new().height(Length::Fill));
    navigation = navigation.push(theme_toggle());
    navigation = navigation.push(
        container(
            text(app.phase.to_uppercase())
                .size(12)
                .color(theme::on_color(theme::phase_color(&app.phase))),
        )
        .style(theme::swatch(theme::phase_color(&app.phase)))
        .padding([3, 10]),
    );
    navigation = navigation.push(
        button(text("New Scan").size(14))
            .on_press(Message::NewScan)
            .width(Length::Fill)
            .padding([8, 14]),
    );

    container(navigation.spacing(6).height(Length::Fill))
        .style(theme::panel)
        .padding(14)
        .width(Length::Fixed(170.0))
        .height(Length::Fill)
        .into()
}

fn header(app: &ScanModel) -> Element<'_, Message> {
    row![
        text(anonymize::path(&app.root))
            .size(15)
            .font(Font::MONOSPACE)
            .color(ui().text),
        text(format_duration(app.counters.elapsed_ms))
            .size(14)
            .color(ui().label),
        Space::new().width(Length::Fill),
        text(format!(
            "{} workers \u{b7} Findex traversal \u{b7} {}",
            app.concurrency
                .map_or_else(|| "auto".to_owned(), |value| value.to_string()),
            app.mount_policy
        ))
        .size(13)
        .color(ui().label),
    ]
    .spacing(14)
    .align_y(Alignment::Center)
    .into()
}

// -------------------------------------------------------- visualization --

fn visualization(app: &ScanModel) -> Element<'_, Message> {
    let chart: Element<'_, Message> = match app.tab {
        Tab::Treemap => treemap_canvas(app),
        Tab::Sunburst => sunburst_canvas(app),
        Tab::Graph => graph_canvas(app),
        Tab::Diagnostics => unreachable!("diagnostics has its own body"),
    };

    let chart_card = container(column![viz_toolbar(app), chart].spacing(10))
        .style(theme::panel)
        .padding(14)
        .width(Length::Fill)
        .height(Length::Fill);

    row![chart_card, inspector(app)]
        .spacing(12)
        .height(Length::Fill)
        .into()
}

fn viz_toolbar(app: &ScanModel) -> Element<'_, Message> {
    // The trail must fit one toolbar line: crumbs use short display
    // names capped at a fixed budget, and a deep chain keeps only the
    // root plus the last few components, with the hidden middle behind
    // an ellipsis that climbs to the deepest hidden ancestor. The exact
    // full path stays available in the inspector.
    const TAIL: usize = 3;
    const NAME_BUDGET: usize = 24;

    let mut breadcrumbs = row![].spacing(2).align_y(Alignment::Center);
    let chain = app.ancestors(app.focus);
    let segments = chain.len();
    let collapsed = segments > TAIL + 2;
    for (position, (id, _)) in chain.iter().enumerate() {
        if collapsed && position > 0 && position < segments - TAIL {
            continue;
        }
        if collapsed && position == segments - TAIL {
            let deepest_hidden = chain[position - 1].0;
            breadcrumbs = breadcrumbs.push(
                button(text("\u{2026}").size(14).color(ui().link))
                    .on_press(Message::FocusDirectory(deepest_hidden))
                    .padding([2, 6])
                    .style(button::text),
            );
            breadcrumbs = breadcrumbs.push(text("/").size(14).color(ui().label));
        }
        let current = position + 1 == segments;
        breadcrumbs = breadcrumbs.push(
            button(
                text(fit_name(
                    &anonymize::name(&app.display_name(*id)),
                    NAME_BUDGET,
                ))
                .size(14)
                .font(Font::MONOSPACE)
                .color(if current { ui().text } else { ui().link }),
            )
            .on_press(Message::FocusDirectory(*id))
            .padding([2, 6])
            .style(button::text),
        );
        if !current {
            breadcrumbs = breadcrumbs.push(text("/").size(14).color(ui().label));
        }
    }

    row![
        breadcrumbs,
        text(human_bytes(app.size_tree.subtree_bytes(app.focus)))
            .size(14)
            .color(ui().green_text),
        Space::new().width(Length::Fill),
        text(match app.tab {
            Tab::Graph => "Click a node to descend \u{b7} drag to pan \u{b7} scroll to zoom",
            _ => "Click to select \u{b7} double-click to drill in",
        })
        .size(12)
        .color(ui().label),
    ]
    .spacing(12)
    .align_y(Alignment::Center)
    .into()
}

fn treemap_canvas(app: &ScanModel) -> Element<'_, Message> {
    let mut cells: Vec<TreemapCell> = app
        .children_by_size(app.focus)
        .into_iter()
        .take(LIST_LIMIT)
        .map(|child| TreemapCell {
            directory_id: child,
            label: anonymize::name(&app.display_name(child)),
            bytes: app.size_tree.subtree_bytes(child),
            drill: !app.size_tree.children(child).is_empty(),
            muted: false,
        })
        .collect();

    // Files directly inside the focus own real bytes too; a muted tile
    // keeps the picture honest instead of hiding them.
    let own_bytes = app.size_tree.own_bytes(app.focus);
    if own_bytes > 0 {
        let position = cells.partition_point(|cell| cell.bytes >= own_bytes);
        cells.insert(
            position,
            TreemapCell {
                directory_id: app.focus,
                label: format!("({} files)", app.size_tree.own_entries(app.focus)),
                bytes: own_bytes,
                drill: false,
                muted: true,
            },
        );
    }

    if cells.is_empty() {
        return viz_empty(app);
    }
    Canvas::new(Treemap {
        cells,
        palette: BRANCH_PALETTE.to_vec(),
    })
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

fn sunburst_canvas(app: &ScanModel) -> Element<'_, Message> {
    const RINGS: u8 = 3;
    /// Slices thinner than ~0.9 degrees aggregate into a quiet filler.
    const MINIMUM_SWEEP: f32 = 0.016;

    // Depth-first angular subdivision: every child owns a span of its
    // parent's angle proportional to its recursive bytes. The remainder
    // (the directory's own files plus sub-minimum children) becomes a
    // muted gray filler so angles remain honest fractions of the focus.
    #[allow(clippy::too_many_arguments)]
    fn subdivide(
        app: &ScanModel,
        slices: &mut Vec<SunburstSlice>,
        id: u32,
        ring: u8,
        start: f32,
        sweep: f32,
        color: Option<Color>,
        rings: u8,
    ) {
        // MINIMUM_SWEEP bounds the real slice count to ~400 per ring, so
        // the length guard is pure runaway protection; a lower cap would
        // silently drop later siblings and leave a blank wedge.
        if ring > rings || sweep < MINIMUM_SWEEP || slices.len() > 2_000 {
            return;
        }
        let total = app.size_tree.subtree_bytes(id).max(1);
        let mut cursor = start;
        for (index, child) in app.children_by_size(id).into_iter().enumerate() {
            let bytes = app.size_tree.subtree_bytes(child);
            let share = sweep * (bytes as f32 / total as f32);
            if share < MINIMUM_SWEEP {
                continue;
            }
            let branch = color.unwrap_or(BRANCH_PALETTE[index % BRANCH_PALETTE.len()]);
            slices.push(SunburstSlice {
                directory_id: Some(child),
                ring,
                start: cursor,
                sweep: share,
                color: mix(branch, ui().panel, 0.18 * f32::from(ring - 1)),
                drill: !app.size_tree.children(child).is_empty(),
                label: anonymize::name(&app.directory_name(child)),
                bytes,
            });
            subdivide(
                app,
                slices,
                child,
                ring + 1,
                cursor,
                share,
                Some(branch),
                rings,
            );
            cursor += share;
        }
        // Everything the loop did not draw — aggregated small children
        // plus the directory's own files — must still cover its span,
        // or the ring shows a blank void. Same deliberate gray as the
        // treemap's muted file tiles: near-white reads as a broken gap.
        let remainder = start + sweep - cursor;
        if remainder > 0.003 {
            slices.push(SunburstSlice {
                directory_id: None,
                ring,
                start: cursor,
                sweep: remainder,
                color: mix(ui().label, ui().panel, 0.45),
                drill: false,
                label: String::new(),
                bytes: 0,
            });
        }
    }

    let mut slices: Vec<SunburstSlice> = Vec::new();
    subdivide(
        app,
        &mut slices,
        app.focus,
        1,
        -std::f32::consts::FRAC_PI_2,
        std::f32::consts::TAU,
        None,
        RINGS,
    );

    if slices.is_empty() {
        return viz_empty(app);
    }
    Canvas::new(Sunburst {
        slices,
        rings: RINGS,
        center_label: anonymize::name(&app.display_name(app.focus)),
        center_bytes: app.size_tree.subtree_bytes(app.focus),
        up: app.size_tree.parent(app.focus),
    })
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

fn graph_canvas(app: &ScanModel) -> Element<'_, Message> {
    use crate::charts::{GraphEdge, GraphLabel, LabelSide};

    /// Children drawn as rows; the rest are counted in a caption.
    const CHILD_LIMIT: usize = 12;
    /// Children that also list their largest grandchildren.
    const PREVIEWED: usize = 4;
    const GRANDCHILDREN_EACH: usize = 3;
    const CHILD_ROW: f32 = 32.0;
    const GRAND_ROW: f32 = 24.0;
    const CHILD_X: f32 = -60.0;
    const GRAND_X: f32 = 150.0;
    const FOCUS_X: f32 = -260.0;
    const TRUNK_X: f32 = -150.0;

    // Rows are child directories plus, when present, one entry for the
    // files sitting directly in the focus — they own real bytes too.
    let own_bytes = app.size_tree.own_bytes(app.focus);
    let mut rows: Vec<(u64, Option<u32>)> = app
        .children_by_size(app.focus)
        .into_iter()
        .map(|child| (app.size_tree.subtree_bytes(child), Some(child)))
        .collect();
    if own_bytes > 0 {
        let position = rows.partition_point(|&(bytes, _)| bytes >= own_bytes);
        rows.insert(position, (own_bytes, None));
    }
    let hidden_children = rows.len().saturating_sub(CHILD_LIMIT);
    let shown: Vec<(u64, Option<u32>)> = rows.into_iter().take(CHILD_LIMIT).collect();
    if shown.is_empty() {
        return viz_empty(app);
    }
    let largest = shown[0].0.max(1);

    let mut nodes: Vec<GraphNode> = Vec::new();
    let mut edges: Vec<GraphEdge> = Vec::new();

    // First pass: assign each child a band tall enough for itself and its
    // listed grandchildren, stacking bands downward from zero.
    struct Band {
        child: Option<u32>,
        bytes: u64,
        child_y: f32,
        grandchildren: Vec<(u32, f32)>,
    }
    let mut bands: Vec<Band> = Vec::new();
    let mut cursor = 0.0_f32;
    for (position, &(bytes, child)) in shown.iter().enumerate() {
        let child_y = cursor + CHILD_ROW / 2.0;
        cursor += CHILD_ROW;
        let mut grandchildren = Vec::new();
        if let Some(child) = child
            && position < PREVIEWED
        {
            for &grandchild in app.children_by_size(child).iter().take(GRANDCHILDREN_EACH) {
                grandchildren.push((grandchild, cursor + GRAND_ROW / 2.0));
                cursor += GRAND_ROW;
            }
        }
        bands.push(Band {
            child,
            bytes,
            child_y,
            grandchildren,
        });
    }
    let offset_y = cursor / 2.0;

    // Ancestor spine above the focus; labels sit left, guides run down.
    let chain = app.ancestors(app.focus);
    let ancestors: Vec<u32> = chain[..chain.len().saturating_sub(1)]
        .iter()
        .rev()
        .take(2)
        .map(|&(id, _)| id)
        .collect();
    for (step, &ancestor) in ancestors.iter().enumerate() {
        let y = -46.0 * (step + 1) as f32;
        nodes.push(GraphNode {
            directory_id: ancestor,
            x: FOCUS_X,
            y,
            radius: 8.0,
            color: DEEP_BLUE,
            alpha: if step == 0 { 0.5 } else { 0.32 },
            label: Some(GraphLabel {
                name: fit_name(&anonymize::name(&app.display_name(ancestor)), 22),
                detail: String::new(),
                side: LabelSide::Left,
            }),
            bytes: app.size_tree.subtree_bytes(ancestor),
            navigates: true,
        });
        edges.push(GraphEdge {
            points: vec![(FOCUS_X, y + 8.0), (FOCUS_X, -18.0)],
        });
    }

    // The focus node, vertically centered on its children block.
    nodes.push(GraphNode {
        directory_id: app.focus,
        x: FOCUS_X,
        y: 0.0,
        radius: 15.0,
        color: SKY_BLUE,
        alpha: 1.0,
        label: Some(GraphLabel {
            name: fit_name(&anonymize::name(&app.display_name(app.focus)), 24),
            detail: human_bytes(app.size_tree.subtree_bytes(app.focus)),
            side: LabelSide::Left,
        }),
        bytes: app.size_tree.subtree_bytes(app.focus),
        navigates: false,
    });

    // Trunk from the focus to the children's guide line.
    // The trunk always spans from the focus junction (y = 0) to the
    // outermost child row; with a single child the rows may sit entirely
    // above or below the junction.
    let first_y = (bands.first().map_or(0.0, |band| band.child_y) - offset_y).min(0.0);
    let last_y = (bands.last().map_or(0.0, |band| band.child_y) - offset_y).max(0.0);
    edges.push(GraphEdge {
        points: vec![(FOCUS_X + 17.0, 0.0), (TRUNK_X, 0.0)],
    });
    edges.push(GraphEdge {
        points: vec![(TRUNK_X, first_y), (TRUNK_X, last_y)],
    });

    for (position, band) in bands.iter().enumerate() {
        let child_y = band.child_y - offset_y;
        let bytes = band.bytes;
        let weight = ((bytes as f32 / largest as f32).sqrt()).clamp(0.25, 1.0);
        let radius = 4.0 + weight * 9.0;
        let (directory_id, color, name, detail, navigates) = match band.child {
            Some(child) => {
                let child_directories = app.size_tree.children(child).len();
                let detail = match child_directories {
                    0 => human_bytes(bytes),
                    1 => format!("{} \u{b7} 1 dir", human_bytes(bytes)),
                    count => format!("{} \u{b7} {count} dirs", human_bytes(bytes)),
                };
                (
                    child,
                    BRANCH_PALETTE[position % BRANCH_PALETTE.len()],
                    fit_name(&anonymize::name(&app.display_name(child)), 26),
                    detail,
                    true,
                )
            }
            None => (
                app.focus,
                mix(ui().label, ui().panel, 0.35),
                format!("({} files)", app.size_tree.own_entries(app.focus)),
                human_bytes(bytes),
                false,
            ),
        };
        nodes.push(GraphNode {
            directory_id,
            x: CHILD_X,
            y: child_y,
            radius,
            color,
            alpha: 1.0,
            label: Some(GraphLabel {
                name,
                detail,
                side: LabelSide::Right,
            }),
            bytes,
            navigates,
        });
        edges.push(GraphEdge {
            points: vec![(TRUNK_X, child_y), (CHILD_X - radius - 2.0, child_y)],
        });

        if band.grandchildren.is_empty() {
            continue;
        }
        let indent_x = CHILD_X + 24.0;
        let last_grand_y = band.grandchildren.last().map_or(child_y, |&(_, y)| y) - offset_y;
        edges.push(GraphEdge {
            points: vec![(indent_x, child_y + radius + 3.0), (indent_x, last_grand_y)],
        });
        for &(grandchild, raw_y) in &band.grandchildren {
            let grand_y = raw_y - offset_y;
            let grand_bytes = app.size_tree.subtree_bytes(grandchild);
            nodes.push(GraphNode {
                directory_id: grandchild,
                x: GRAND_X,
                y: grand_y,
                radius: 4.0,
                color: mix(color, ui().panel, 0.25),
                alpha: 0.9,
                label: Some(GraphLabel {
                    name: fit_name(&anonymize::name(&app.display_name(grandchild)), 22),
                    detail: human_bytes(grand_bytes),
                    side: LabelSide::Right,
                }),
                bytes: grand_bytes,
                navigates: true,
            });
            edges.push(GraphEdge {
                points: vec![(indent_x, grand_y), (GRAND_X - 6.0, grand_y)],
            });
        }
    }

    Canvas::new(NodeGraph {
        nodes,
        edges,
        hidden_children,
        focus: app.focus,
    })
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

/// Truncates a name for a graph label.
fn fit_name(name: &str, budget: usize) -> String {
    if name.chars().count() <= budget {
        return name.to_owned();
    }
    let mut fitted: String = name.chars().take(budget.saturating_sub(1)).collect();
    fitted.push('\u{2026}');
    fitted
}

fn viz_empty(app: &ScanModel) -> Element<'_, Message> {
    let own_bytes = app.size_tree.own_bytes(app.focus);
    let message = if own_bytes > 0 {
        format!(
            "No subdirectories \u{2014} {} across {} files stored directly here.",
            human_bytes(own_bytes),
            app.size_tree.own_entries(app.focus),
        )
    } else {
        "No sized contents in this directory yet.".to_owned()
    };
    container(
        column![
            text(message).size(14).color(ui().label),
            button(text("Reveal in Finder").size(13))
                .on_press(Message::Reveal(app.full_path(app.focus))),
        ]
        .spacing(10)
        .align_x(Alignment::Center),
    )
    .center_x(Length::Fill)
    .center_y(Length::Fill)
    .into()
}

// ------------------------------------------------------------ inspector --

fn inspector(app: &ScanModel) -> Element<'_, Message> {
    let subject = app.inspected();
    let bytes = app.size_tree.subtree_bytes(subject);
    let focus_bytes = app.size_tree.subtree_bytes(app.focus).max(1);
    let share = 100.0 * bytes as f64 / focus_bytes as f64;

    let mut content = column![
        section_label("SELECTION"),
        text(anonymize::name(&app.display_name(subject)))
            .size(17)
            .font(Font::MONOSPACE),
        text(anonymize::path(&app.full_path(subject)))
            .size(11)
            .font(Font::MONOSPACE)
            .color(ui().label),
        row![
            text(human_bytes(bytes)).size(22).color(ui().link),
            text(if subject == app.focus {
                "100% \u{b7} focus".to_owned()
            } else {
                format!("{share:.1}% of focus")
            })
            .size(12)
            .color(ui().label),
        ]
        .spacing(10)
        .align_y(Alignment::End),
        text(format!(
            "{} child directories",
            app.size_tree.children(subject).len()
        ))
        .size(12)
        .color(ui().label),
        text(format!(
            "{} across {} files stored directly here",
            human_bytes(app.size_tree.own_bytes(subject)),
            app.size_tree.own_entries(subject),
        ))
        .size(12)
        .color(ui().label),
    ]
    .spacing(6);

    // Direct children, largest first, as a compact clickable breakdown.
    let children = app.children_by_size(subject);
    if !children.is_empty() {
        let largest = app.size_tree.subtree_bytes(children[0]).max(1);
        let mut breakdown = column![].spacing(4);
        for child in children.iter().take(8).copied() {
            let child_bytes = app.size_tree.subtree_bytes(child);
            let fraction = child_bytes as f32 / largest as f32;
            breakdown = breakdown.push(
                mouse_area(
                    column![
                        row![
                            text(anonymize::name(&app.directory_name(child)))
                                .size(12)
                                .font(Font::MONOSPACE),
                            Space::new().width(Length::Fill),
                            text(human_bytes(child_bytes)).size(12).color(ui().label),
                        ]
                        .spacing(6),
                        container(
                            Space::new()
                                .width(Length::Fixed((fraction * 250.0).max(2.0)))
                                .height(Length::Fixed(4.0))
                        )
                        .style(theme::swatch(SKY_BLUE)),
                    ]
                    .spacing(2),
                )
                .interaction(mouse::Interaction::Pointer)
                .on_press(Message::NodeSelected(child)),
            );
        }
        content = content.push(section_label("LARGEST CHILDREN"));
        content = content.push(breakdown);
    }

    // Largest known files inside this subtree, from the global top list;
    // clicking reveals the file itself in Finder.
    let prefix = format!("{}/", app.full_path(subject));
    let mut subtree_files = app
        .top_files
        .iter()
        .filter(|entry| entry.path.starts_with(&prefix))
        .take(6)
        .peekable();
    if subtree_files.peek().is_some() {
        let mut list = column![].spacing(3);
        for entry in subtree_files {
            let name = entry
                .path
                .rsplit('/')
                .next()
                .unwrap_or(&entry.path)
                .to_owned();
            list = list.push(
                mouse_area(
                    row![
                        text(fit_name(&anonymize::name(&name), 22))
                            .size(12)
                            .font(Font::MONOSPACE),
                        Space::new().width(Length::Fill),
                        text(human_bytes(entry.bytes)).size(12).color(ui().label),
                    ]
                    .spacing(6),
                )
                .interaction(mouse::Interaction::Pointer)
                .on_press(Message::Reveal(entry.path.clone())),
            );
        }
        content = content.push(section_label("LARGEST FILES INSIDE"));
        content = content.push(list);
    }

    // Focus/up navigation lives in the charts and breadcrumbs; the only
    // action gestures cannot do is leaving the app for Finder.
    content = content.push(Space::new().height(Length::Fill));
    content = content.push(
        button(text("Reveal in Finder").size(13))
            .on_press(Message::Reveal(app.full_path(subject)))
            .width(Length::Fill)
            .padding([7, 12]),
    );

    container(content.height(Length::Fill))
        .style(theme::panel)
        .padding(14)
        .width(Length::Fixed(300.0))
        .height(Length::Fill)
        .into()
}

// ---------------------------------------------------------- diagnostics --

fn diagnostics(app: &ScanModel) -> Element<'_, Message> {
    let counters = &app.counters;
    let coverage = if counters.directories_reserved == 0 {
        0.0
    } else {
        (counters.directories_completed as f64 / counters.directories_reserved as f64)
            .clamp(0.0, 1.0)
    };

    let kpis = row![
        kpi_spark(
            "ENTRIES",
            human_count(counters.entries),
            format!(
                "{}/s average",
                human_count(counters.entries_per_second as u64)
            ),
            app.throughput_history.iter().copied().collect(),
            SKY_BLUE,
        ),
        kpi_plain(
            "ALLOCATED SIZE",
            human_bytes(counters.allocated_bytes),
            format!("{} regular files", human_count(counters.regular_files)),
        ),
        kpi_progress(
            "COVERAGE",
            format!("{:.1}%", coverage * 100.0),
            format!(
                "{} / {} directories",
                human_count(counters.directories_completed),
                human_count(counters.directories_reserved)
            ),
            coverage as f32,
            theme::phase_color(&app.phase),
        ),
        kpi_spark(
            "QUEUE",
            human_count(counters.scheduler_pending + counters.in_flight),
            format!("{} in flight", human_count(counters.in_flight)),
            app.queue_history.iter().copied().collect(),
            DEEP_BLUE,
        ),
        kpi_value_colored(
            "FAILURES",
            human_count(counters.directories_failed),
            format!("{} metadata errors", counters.metadata_errors),
            if counters.directories_failed > 0 {
                SIGNAL_RED
            } else {
                theme::TEAL_GREEN
            },
        ),
    ]
    .spacing(12)
    .height(Length::Fixed(110.0));

    let rate_chart = card(
        "INDEXING RATE \u{b7} LAST MINUTE",
        Canvas::new(AreaChart {
            samples: app.throughput_history.iter().copied().collect(),
            color: SKY_BLUE,
            format: |value| format!("{}/s", human_count(value)),
        })
        .width(Length::Fill)
        .height(Length::Fill)
        .into(),
    );

    let donut = card(
        "ENTRY TYPES",
        row![
            Canvas::new(Donut {
                slices: vec![
                    (counters.regular_files, SKY_BLUE),
                    (counters.directory_entries, DEEP_BLUE),
                    (counters.symlinks, theme::BLOSSOM_PINK),
                    (counters.other, AMBER),
                ],
                center_title: "entries".to_owned(),
                center_value: human_count(counters.entries),
            })
            .width(Length::Fill)
            .height(Length::Fill),
            column![
                legend_row(SKY_BLUE, "files", counters.regular_files),
                legend_row(DEEP_BLUE, "dirs", counters.directory_entries),
                legend_row(theme::BLOSSOM_PINK, "symlinks", counters.symlinks),
                legend_row(AMBER, "other", counters.other),
            ]
            .spacing(8)
            .width(Length::Fixed(130.0)),
        ]
        .spacing(8)
        .align_y(Alignment::Center)
        .into(),
    );

    let histogram = card(
        "FILE SIZE DISTRIBUTION \u{b7} \u{221a}COUNT",
        Canvas::new(Histogram {
            counts: app.size_histogram.counts.to_vec(),
            color: SKY_BLUE,
        })
        .width(Length::Fill)
        .height(Length::Fill)
        .into(),
    );

    let memory = card(
        "STORE MEMORY",
        column![
            Canvas::new(AreaChart {
                samples: app.memory_history.iter().copied().collect(),
                color: DEEP_BLUE,
                format: human_bytes,
            })
            .width(Length::Fill)
            .height(Length::Fill),
            row![
                memory_stat("blocks", counters.block_bytes),
                memory_stat("payload", counters.payload_bytes),
                memory_stat("table", counters.directory_table_bytes),
                memory_stat("journal", counters.journal_bytes),
            ]
            .spacing(12),
        ]
        .spacing(8)
        .into(),
    );

    let feed = card(
        "COMPLETION FEED \u{b7} CLICK TO REVEAL IN FINDER",
        column![
            text_input("Filter paths\u{2026}", &app.filter)
                .on_input(Message::FilterChanged)
                .padding(6)
                .size(13),
            completions_list(app),
        ]
        .spacing(8)
        .into(),
    );

    let mut files = column![].spacing(2);
    for entry in app.top_files.iter().take(LIST_LIMIT) {
        files = files.push(
            mouse_area(
                container(
                    row![
                        text(human_bytes(entry.bytes))
                            .size(12)
                            .width(Length::Fixed(70.0)),
                        text(anonymize::path(&entry.path))
                            .size(12)
                            .font(Font::MONOSPACE),
                    ]
                    .spacing(8),
                )
                .padding([2, 4])
                .width(Length::Fill),
            )
            .interaction(mouse::Interaction::Pointer)
            .on_press(Message::Reveal(entry.path.clone())),
        );
    }
    let largest = card(
        "LARGEST FILES \u{b7} CLICK TO REVEAL IN FINDER",
        scrollable(files).height(Length::Fill).into(),
    );

    let body = column![
        kpis,
        row![
            container(rate_chart).width(Length::FillPortion(5)),
            container(donut).width(Length::FillPortion(3)),
        ]
        .spacing(12)
        .height(Length::Fixed(230.0)),
        row![histogram, memory]
            .spacing(12)
            .height(Length::Fixed(210.0)),
        failures_section(app),
        row![
            container(feed).width(Length::FillPortion(5)),
            container(largest).width(Length::FillPortion(4)),
        ]
        .spacing(12)
        .height(Length::Fixed(320.0)),
    ]
    .spacing(12)
    .padding(iced::Padding {
        right: 14.0,
        ..iced::Padding::ZERO
    });

    scrollable(body).height(Length::Fill).into()
}

fn completions_list(app: &ScanModel) -> Element<'_, Message> {
    let directories = app.filtered_recent();
    let mut list = column![].spacing(2);
    for (directory, real_path) in directories.iter().take(LIST_LIMIT) {
        let path = if directory.error.is_empty() {
            anonymize::path(real_path)
        } else {
            format!("{}  [{}]", anonymize::path(real_path), directory.error)
        };
        list = list.push(
            mouse_area(
                container(
                    row![
                        text(directory.state.clone())
                            .size(12)
                            .color(theme::state_color(&directory.state))
                            .width(Length::Fixed(70.0)),
                        text(format!("{} / {}", directory.entries, directory.children))
                            .size(12)
                            .color(ui().label)
                            .width(Length::Fixed(64.0)),
                        text(path).size(12).font(Font::MONOSPACE),
                    ]
                    .spacing(8),
                )
                .padding([2, 4])
                .width(Length::Fill),
            )
            .interaction(mouse::Interaction::Pointer)
            .on_press(Message::Reveal(real_path.clone())),
        );
    }
    scrollable(list).height(Length::Fill).into()
}

fn failures_section(app: &ScanModel) -> Element<'_, Message> {
    if app.counters.directories_failed == 0 && app.counters.metadata_errors == 0 {
        return container(
            text("No directory failures or metadata errors observed.")
                .size(13)
                .color(theme::TEAL_GREEN),
        )
        .style(theme::panel)
        .padding(14)
        .width(Length::Fill)
        .into();
    }

    let counts = row![
        bars_card(
            format!("METADATA ERRORS \u{b7} {}", app.counters.metadata_errors),
            &app.metadata_error_counts,
            AMBER,
        ),
        bars_card(
            "FAILURE CATEGORIES".to_owned(),
            &app.directory_failure_counts,
            SIGNAL_RED,
        ),
        bars_card(
            "FAILURE REASONS".to_owned(),
            &app.directory_failure_reasons,
            SIGNAL_RED,
        ),
    ]
    .spacing(12)
    .height(Length::Fixed(180.0));

    if app.failed_directories.is_empty() {
        return counts.into();
    }

    let mut list = column![].spacing(3);
    for failure in app.failed_directories.iter().take(LIST_LIMIT * 2) {
        let detail = [failure.phase.as_str(), failure.category.as_str()]
            .iter()
            .filter(|part| !part.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join(" \u{b7} ");
        list = list.push(
            mouse_area(
                container(
                    row![
                        text(failure.reason.clone())
                            .size(13)
                            .color(SIGNAL_RED)
                            .width(Length::Fixed(110.0)),
                        text(detail)
                            .size(13)
                            .color(ui().label)
                            .width(Length::Fixed(170.0)),
                        text(anonymize::path(&failure.path))
                            .size(13)
                            .font(Font::MONOSPACE),
                    ]
                    .spacing(8),
                )
                .padding([2, 6])
                .width(Length::Fill),
            )
            .interaction(mouse::Interaction::Pointer)
            .on_press(Message::Reveal(failure.path.clone())),
        );
    }
    let failed = card(
        "FAILED DIRECTORIES \u{b7} CLICK TO REVEAL IN FINDER",
        scrollable(list).height(Length::Fill).into(),
    );

    column![counts, container(failed).height(Length::Fixed(220.0))]
        .spacing(12)
        .into()
}

fn legend_row(color: Color, label: &str, count: u64) -> Element<'static, Message> {
    row![
        container(
            Space::new()
                .width(Length::Fixed(10.0))
                .height(Length::Fixed(10.0))
        )
        .style(theme::swatch(color)),
        text(format!("{label} {}", human_count(count)))
            .size(13)
            .color(ui().label),
    ]
    .spacing(6)
    .align_y(Alignment::Center)
    .into()
}

fn memory_stat(label: &'static str, bytes: u64) -> Element<'static, Message> {
    column![
        text(label).size(11).color(ui().label),
        text(human_bytes(bytes)).size(13),
    ]
    .spacing(2)
    .into()
}

fn bars_card<'a>(
    title: String,
    counts: &'a std::collections::BTreeMap<String, u64>,
    color: Color,
) -> Element<'a, Message> {
    let mut sorted: Vec<_> = counts.iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    let maximum = sorted.first().map_or(1, |entry| (*entry.1).max(1));

    let mut list = column![].spacing(6);
    if sorted.is_empty() {
        list = list.push(text("none").size(13).color(ui().label));
    }
    for (key, &count) in sorted.into_iter().take(6) {
        let fraction = count as f32 / maximum as f32;
        list = list.push(
            row![
                text(key.clone()).size(12).width(Length::Fixed(110.0)),
                container(
                    Space::new()
                        .width(Length::Fixed((fraction * 140.0).max(2.0)))
                        .height(Length::Fixed(10.0))
                )
                .style(theme::swatch(color))
                .width(Length::Fixed(144.0)),
                text(human_count(count)).size(12).color(ui().label),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        );
    }

    container(
        column![text(title).size(11).color(ui().label), list]
            .spacing(10)
            .height(Length::Fill),
    )
    .style(theme::panel)
    .padding(14)
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

// ---------------------------------------------------------------- cards --

fn card<'a>(title: &'a str, body: Element<'a, Message>) -> Element<'a, Message> {
    container(
        column![text(title).size(11).color(ui().label), body]
            .spacing(8)
            .height(Length::Fill),
    )
    .style(theme::panel)
    .padding(14)
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

fn kpi_frame(body: Element<'_, Message>) -> Element<'_, Message> {
    container(body)
        .style(theme::panel)
        .padding(14)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

fn kpi_spark(
    title: &'static str,
    value: String,
    sub: String,
    samples: Vec<u64>,
    color: Color,
) -> Element<'static, Message> {
    kpi_frame(
        row![
            column![
                text(title).size(11).color(ui().label),
                text(value).size(26),
                text(sub).size(12).color(ui().label),
            ]
            .spacing(3),
            Canvas::new(Sparkline { samples, color })
                .width(Length::Fill)
                .height(Length::Fill),
        ]
        .spacing(10)
        .align_y(Alignment::End)
        .into(),
    )
}

fn kpi_plain(title: &'static str, value: String, sub: String) -> Element<'static, Message> {
    kpi_frame(
        column![
            text(title).size(11).color(ui().label),
            text(value).size(26),
            text(sub).size(12).color(ui().label),
        ]
        .spacing(3)
        .into(),
    )
}

fn kpi_value_colored(
    title: &'static str,
    value: String,
    sub: String,
    color: Color,
) -> Element<'static, Message> {
    kpi_frame(
        column![
            text(title).size(11).color(ui().label),
            text(value).size(26).color(color),
            text(sub).size(12).color(ui().label),
        ]
        .spacing(3)
        .into(),
    )
}

fn kpi_progress(
    title: &'static str,
    value: String,
    sub: String,
    ratio: f32,
    color: Color,
) -> Element<'static, Message> {
    kpi_frame(
        column![
            text(title).size(11).color(ui().label),
            text(value).size(26),
            progress_bar(0.0..=1.0, ratio)
                .girth(6)
                .style(move |_theme: &Theme| progress_bar::Style {
                    background: iced::Background::Color(ui().panel_highlight),
                    bar: iced::Background::Color(color),
                    border: Border {
                        radius: 3.0.into(),
                        ..Border::default()
                    },
                }),
            text(sub).size(12).color(ui().label),
        ]
        .spacing(4)
        .into(),
    )
}
