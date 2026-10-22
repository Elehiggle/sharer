//! Bounded frame delivery to an external H.264 encoder.

use std::{
    io::{Read, Seek as _, SeekFrom, Write as _},
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};
use tempfile::NamedTempFile;

use super::{FrameWriter, MAX_RECORDING_BYTES};
use sharer::capture::recording_frame_bytes;

const ENCODER_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_DIAGNOSTIC_BYTES: usize = 8192;
// FFmpeg's -fs can overshoot by a packet and final muxing data. Reject any
// oversized result as well, so it can never enter the save/upload pipeline.
const FILE_LIMIT: u64 = MAX_RECORDING_BYTES - 1024 * 1024;

pub(super) struct Mp4Writer {
    dimensions: (u32, u32),
    child: Child,
    input: Option<SyncSender<Vec<u8>>>,
    writer: Option<JoinHandle<std::io::Result<()>>>,
    diagnostic: Option<JoinHandle<String>>,
    output: Option<NamedTempFile>,
    started: Instant,
    progress: Arc<AtomicU64>,
    file_limit: u64,
}

impl Mp4Writer {
    pub(super) fn new(dimensions: (u32, u32), frames_per_second: u32) -> Result<Self> {
        Self::with_limit(dimensions, frames_per_second, FILE_LIMIT)
    }

    fn with_limit(dimensions: (u32, u32), frames_per_second: u32, file_limit: u64) -> Result<Self> {
        recording_frame_bytes(dimensions)?;
        sharer::validate_recording_fps(frames_per_second)?;
        let executable = super::ffmpeg::find().context(
            "FFmpeg was not found; install it or place it beside ShareR, or turn off MP4 recording",
        )?;
        let output = tempfile::Builder::new()
            .prefix("sharer-recording-")
            .suffix(".mp4")
            .tempfile()
            .context("failed to create a temporary MP4 file")?;
        let child = encoder_command(&executable, dimensions, frames_per_second, file_limit, output.path()).spawn()
            .with_context(|| format!("could not start FFmpeg at {}; install FFmpeg with libx264 or turn off MP4 recording", executable.display()))?;
        let mut encoder = Self {
            dimensions,
            child,
            input: None,
            writer: None,
            diagnostic: None,
            output: Some(output),
            started: Instant::now(),
            progress: Arc::new(AtomicU64::new(0)),
            file_limit,
        };
        let stderr = encoder
            .child
            .stderr
            .take()
            .context("FFmpeg stderr unavailable")?;

        encoder.diagnostic = Some(
            thread::Builder::new()
                .name("sharer-ffmpeg-errors".into())
                .spawn(move || diagnostic_tail(stderr))?,
        );
        let mut stdin = encoder
            .child
            .stdin
            .take()
            .context("FFmpeg stdin unavailable")?;
        // At most one queued frame, one being written, and one being captured.
        // A slow encoder drops new frames instead of building up a recording in RAM.
        let (sender, receiver) = mpsc::sync_channel::<Vec<u8>>(1);
        let progress = Arc::clone(&encoder.progress);
        let started = encoder.started;

        encoder.writer = Some(
            thread::Builder::new()
                .name("sharer-ffmpeg-input".into())
                .spawn(move || {
                    while let Ok(frame) = receiver.recv() {
                        stdin.write_all(&frame)?;
                        progress.store(started.elapsed().as_secs(), Ordering::Relaxed);
                    }

                    Ok(())
                })?,
        );
        encoder.input = Some(sender);

        Ok(encoder)
    }

    fn output_length(&self) -> Result<u64> {
        Ok(self
            .output
            .as_ref()
            .context("MP4 output unavailable")?
            .as_file()
            .metadata()?
            .len())
    }
}

fn encoder_command(
    executable: &Path,
    dimensions: (u32, u32),
    fps: u32,
    file_limit: u64,
    output: &Path,
) -> Command {
    let mut command = Command::new(executable);

    command.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostats",
        "-nostdin",
        "-y",
    ]);
    command.args([
        "-filter_threads",
        "1",
        "-f",
        "rawvideo",
        "-pixel_format",
        "rgba",
    ]);
    command.args(["-video_size", &format!("{}x{}", dimensions.0, dimensions.1)]);
    command.args(["-framerate", &fps.to_string()]);
    // Rawvideo's default frame-count timestamps would speed up recordings when
    // capture falls behind. Timestamp packet arrival instead.
    command.args([
        "-use_wallclock_as_timestamps",
        "1",
        "-probesize",
        "32",
        "-analyzeduration",
        "0",
    ]);
    command.args([
        "-thread_queue_size",
        "1",
        "-threads",
        "1",
        "-i",
        "pipe:0",
        "-an",
    ]);
    command.args(["-c:v", "libx264", "-threads", "2", "-preset", "veryfast"]);
    command.args([
        "-tune",
        "zerolatency",
        "-crf",
        "23",
        "-bf",
        "0",
        "-refs",
        "1",
        "-rc-lookahead",
        "0",
    ]);
    command.args([
        "-vf",
        "pad=ceil(iw/2)*2:ceil(ih/2)*2",
        "-pix_fmt",
        "yuv420p",
    ]);
    command.args(["-fps_mode", "vfr", "-g", &(fps * 2).to_string()]);
    // Small fragments and no global index keep muxing memory independent of
    // recording length. Each fragment's media and packet index can be released.
    command.args([
        "-movflags",
        "+frag_keyframe+empty_moov+default_base_moof+skip_trailer",
    ]);
    command.args(["-frag_duration", "1000000", "-frag_size", "1048576"]);
    command.args(["-fs", &file_limit.to_string(), "-f", "mp4"]);
    command
        .arg(output)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;

        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }

    command
}

impl FrameWriter for Mp4Writer {
    type Output = NamedTempFile;

    fn dimensions(&self) -> (u32, u32) {
        self.dimensions
    }

    fn add_rgba(&mut self, image: Vec<u8>, _timestamp: i32) -> Result<bool> {
        anyhow::ensure!(
            image.len() == recording_frame_bytes(self.dimensions)?,
            "screen capture returned an unexpected recording size"
        );

        if let Some(status) = self.child.try_wait()? {
            if status.success() && self.output_length()? >= self.file_limit {
                return Ok(false);
            }

            let diagnostic = self
                .diagnostic
                .take()
                .and_then(|worker| worker.join().ok())
                .unwrap_or_default();

            anyhow::bail!(
                "FFmpeg stopped before recording finished (requires libx264): {diagnostic}"
            );
        }

        anyhow::ensure!(
            self.started
                .elapsed()
                .as_secs()
                .saturating_sub(self.progress.load(Ordering::Relaxed))
                < ENCODER_TIMEOUT.as_secs(),
            "FFmpeg stopped accepting recording frames"
        );

        match self
            .input
            .as_ref()
            .context("FFmpeg input unavailable")?
            .try_send(image)
        {
            Ok(()) | Err(TrySendError::Full(_)) => Ok(true),
            Err(TrySendError::Disconnected(_)) => Ok(false),
        }
    }

    fn finish(mut self, _timestamp: i32) -> Result<(NamedTempFile, u64)> {
        // Closing the channel drains the bounded queue, then closes stdin so
        // FFmpeg can finish its last fragment. Never wait indefinitely on a codec.
        self.input.take();
        let deadline = Instant::now() + ENCODER_TIMEOUT;
        let status = loop {
            if let Some(status) = self.child.try_wait()? {
                break status;
            }

            anyhow::ensure!(
                Instant::now() < deadline,
                "FFmpeg timed out finishing the recording"
            );
            thread::sleep(Duration::from_millis(20));
        };
        let input_result = self
            .writer
            .take()
            .context("FFmpeg input worker unavailable")?
            .join()
            .map_err(|_| anyhow::anyhow!("FFmpeg input worker stopped unexpectedly"))?;
        let diagnostic = self
            .diagnostic
            .take()
            .context("FFmpeg diagnostic worker unavailable")?
            .join()
            .map_err(|_| anyhow::anyhow!("FFmpeg diagnostic worker stopped unexpectedly"))?;

        anyhow::ensure!(
            status.success(),
            "FFmpeg could not encode MP4 (requires libx264): {diagnostic}"
        );
        let length = self.output_length()?;

        // Reaching -fs intentionally closes the pipe before all queued input is read.
        if length < self.file_limit {
            input_result.context("failed to send recording frames to FFmpeg")?;
        }

        anyhow::ensure!(length > 0, "FFmpeg produced an empty recording");
        anyhow::ensure!(
            length <= MAX_RECORDING_BYTES,
            "MP4 recording exceeds the 256 MiB file limit"
        );
        let mut output = self.output.take().context("MP4 output unavailable")?;

        output.seek(SeekFrom::Start(0))?;

        Ok((output, length))
    }
}

impl Drop for Mp4Writer {
    fn drop(&mut self) {
        // Killing first also unblocks a pipe writer on every early-error path.
        #[cfg(windows)]
        if matches!(self.child.try_wait(), Ok(None)) {
            use std::os::windows::process::CommandExt as _;

            // PATH can resolve to a launcher (for example, a Scoop shim).
            // Terminate its encoder child too, before joining the pipe workers.
            let _ = Command::new("taskkill")
                .args(["/PID", &self.child.id().to_string(), "/T", "/F"])
                .creation_flags(0x0800_0000)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }

        let _ = self.child.kill();
        let _ = self.child.wait();
        self.input.take();

        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }

        if let Some(diagnostic) = self.diagnostic.take() {
            let _ = diagnostic.join();
        }
    }
}

fn diagnostic_tail(mut reader: impl Read) -> String {
    let mut tail = Vec::with_capacity(MAX_DIAGNOSTIC_BYTES);
    let mut buffer = [0; 4096];

    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                let excess = (tail.len() + count).saturating_sub(MAX_DIAGNOSTIC_BYTES);

                tail.drain(..excess);
                tail.extend_from_slice(&buffer[..count]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }

    String::from_utf8_lossy(&tail).trim().to_owned()
}

#[cfg(test)]
mod tests;
