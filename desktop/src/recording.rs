//! Bounded PNG writing off the UI thread, with capture-time metadata.

use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::thread::JoinHandle;
use std::time::Duration;

use iced::window::Screenshot;

struct Frame {
    index: usize,
    elapsed: Duration,
    shot: Screenshot,
}

pub struct FrameWriter {
    sender: Option<SyncSender<Frame>>,
    worker: Option<JoinHandle<Result<(), String>>>,
    frames: usize,
    dropped: usize,
}

impl FrameWriter {
    pub fn new(directory: PathBuf) -> Self {
        // Bound both memory and disk backlog; recording never blocks rendering.
        let (sender, receiver) = sync_channel::<Frame>(4);
        let worker = std::thread::spawn(move || {
            std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(directory.join("frames.jsonl"))
                .map_err(|error| error.to_string())?;
            let mut manifest = BufWriter::new(file);
            for frame in receiver {
                let name = format!("frame-{:04}.png", frame.index);
                crate::save_png(&directory.join(&name), &frame.shot)?;
                serde_json::to_writer(
                    &mut manifest,
                    &serde_json::json!({
                        "file": name,
                        "elapsed_seconds": frame.elapsed.as_secs_f64(),
                    }),
                )
                .map_err(|error| error.to_string())?;
                writeln!(manifest).map_err(|error| error.to_string())?;
            }
            manifest.flush().map_err(|error| error.to_string())
        });
        Self {
            sender: Some(sender),
            worker: Some(worker),
            frames: 0,
            dropped: 0,
        }
    }

    pub fn push(&mut self, shot: Screenshot, elapsed: Duration) -> Result<(), String> {
        let frame = Frame {
            index: self.frames,
            elapsed,
            shot,
        };
        match self
            .sender
            .as_ref()
            .ok_or_else(|| "recording writer is closed".to_owned())?
            .try_send(frame)
        {
            Ok(()) => self.frames += 1,
            Err(TrySendError::Full(_)) => self.dropped += 1,
            Err(TrySendError::Disconnected(_)) => {
                self.finish()?;
                return Err("recording writer disconnected".to_owned());
            }
        }
        Ok(())
    }

    pub fn finish(&mut self) -> Result<(), String> {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| "recording writer panicked".to_owned())??;
            eprintln!(
                "essm: recorded {} frames ({} dropped by writer)",
                self.frames, self.dropped
            );
        }
        Ok(())
    }
}

impl Drop for FrameWriter {
    fn drop(&mut self) {
        if let Err(error) = self.finish() {
            eprintln!("essm: could not finish recording: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finishing_drains_frames_and_preserves_capture_times_and_pixels() {
        let directory = std::env::temp_dir().join(format!(
            "essm-recording-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let pixels = [11, 20, 30, 255].repeat(4);
        let mut writer = FrameWriter::new(directory.clone());
        for millis in [100, 134] {
            writer
                .push(
                    Screenshot::new(pixels.clone(), iced::Size::new(2, 2), 1.0),
                    Duration::from_millis(millis),
                )
                .unwrap();
        }
        writer.finish().unwrap();
        assert!(
            writer
                .push(
                    Screenshot::new(pixels.clone(), iced::Size::new(2, 2), 1.0),
                    Duration::from_millis(168),
                )
                .is_err()
        );
        let entries: Vec<serde_json::Value> =
            std::fs::read_to_string(directory.join("frames.jsonl"))
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["elapsed_seconds"], 0.1);
        assert_eq!(entries[1]["elapsed_seconds"], 0.134);
        let image = std::fs::File::open(directory.join("frame-0001.png")).unwrap();
        let mut reader = png::Decoder::new(image).read_info().unwrap();
        let mut decoded = vec![0; reader.output_buffer_size()];
        reader.next_frame(&mut decoded).unwrap();
        assert_eq!(decoded, pixels);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
