//! Speaker output through WASAPI directly, at the shortest engine period the speaker allows
//! (IAudioClient3), instead of through cpal.
//!
//! cpal opens shared-mode streams with IAudioClient::Initialize, which leaves Windows' 10 ms
//! period and 22 ms buffer: 33 ms from writing a frame until it plays, on the speakers this was
//! measured on. At their shortest period (336 frames, 7 ms) it was 23 ms, and the speaker takes
//! sound every 7 ms instead of 10, so the buffer ahead of it can be smaller too. Opening fails if
//! the speaker can't run at its shortest period (e.g. another program already plays on it
//! directly, at another one).
//!
//! Each stream runs on its own thread, waiting for Windows to ask for sound, the same way cpal
//! does it. The COM objects live on that thread.

use crate::devices;
use anyhow::{anyhow, bail, Context, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::Arc;
use std::thread::JoinHandle;
use windows::core::{w, GUID};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    IAudioClient, IAudioClient3, IAudioClock, IAudioRenderClient,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::System::Com::{
    CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW,
    WaitForSingleObject,
};

/// How long the speaker may go without asking for sound before the stream counts as failed
const STALL_MS: u32 = 2000;
/// How often the thread checks whether it should stop, while waiting
const WAIT_MS: u32 = 100;

/// The speaker's format, and how the stream was opened
#[derive(Debug, Clone)]
pub struct Format {
    /// The speaker's name, as Windows shows it
    pub name: String,
    pub rate: u32,
    pub channels: usize,
    /// How often the speaker asks for sound, in frames
    pub period: u32,
}

/// When the sound handed over in one callback will play
pub struct Timing {
    /// When its first frame will be heard, in 100 ns ticks of the performance counter (the
    /// clock WASAPI time stamps capture with)
    pub playback: u64,
    /// How long that is from now, in s
    pub out_lat: f64,
    /// The speaker had played everything it was given (a gap in the sound)
    pub underrun: bool,
}

/// Fills one callback's block, interleaved, every frame
pub type Data = Box<dyn FnMut(&mut [f32], &Timing) + Send>;
/// Called once if the stream fails (e.g. the speaker was unplugged)
pub type OnError = Box<dyn FnOnce(anyhow::Error) + Send>;

/// One speaker's stream. Dropping it stops it, waiting for its thread.
pub struct Output {
    format: Format,
    start: Option<SyncSender<(Data, OnError)>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Output {
    /// Open the playback device matching `pattern` (chosen as by [`devices::pick`]). It plays
    /// nothing until [`Output::start`].
    pub fn open(pattern: &str) -> Result<Output> {
        let (opened_tx, opened_rx) = mpsc::sync_channel(1);
        let (start_tx, start_rx) = mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let pattern = pattern.to_string();
        let thread = std::thread::Builder::new()
            .name("output".into())
            .spawn(move || run(&pattern, opened_tx, start_rx, &thread_stop))
            .context("Failed to start the output thread")?;
        let format = match opened_rx.recv() {
            Ok(Ok(format)) => format,
            Ok(Err(e)) => {
                let _ = thread.join();
                return Err(e);
            }
            Err(_) => {
                let _ = thread.join();
                bail!("The output thread ended unexpectedly");
            }
        };
        Ok(Output {
            format,
            start: Some(start_tx),
            stop,
            thread: Some(thread),
        })
    }

    pub fn format(&self) -> &Format {
        &self.format
    }

    /// Start playing what `data` hands out; `on_error` is called if the stream fails
    pub fn start(&mut self, data: Data, on_error: OnError) -> Result<()> {
        self.start
            .take()
            .ok_or_else(|| anyhow!("The output was already started"))?
            .send((data, on_error))
            .map_err(|_| anyhow!("The output thread ended unexpectedly"))
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Wakes the thread if it was never started
        self.start = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Undoes what the thread set up, in reverse order, however it ends
struct Cleanup {
    event: Option<HANDLE>,
    mix_format: Option<*mut WAVEFORMATEX>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        unsafe {
            if let Some(event) = self.event {
                let _ = CloseHandle(event);
            }
            if let Some(format) = self.mix_format {
                CoTaskMemFree(Some(format as *const _));
            }
            CoUninitialize();
        }
    }
}

/// The stream's thread: open the speaker, report its format, wait to be started, then play
/// until stopped or failed
fn run(
    pattern: &str,
    opened: SyncSender<Result<Format>>,
    start: Receiver<(Data, OnError)>,
    stop: &AtomicBool,
) {
    // Dropped last (declared first), after the COM objects in `stream`
    let mut cleanup = Cleanup {
        event: None,
        mix_format: None,
    };
    let stream = unsafe {
        if let Err(e) = CoInitializeEx(None, COINIT_MULTITHREADED).ok() {
            let _ = opened.send(Err(e.into()));
            return;
        }
        Stream::open(pattern, &mut cleanup)
    };
    let stream = match stream {
        Ok(stream) => {
            let _ = opened.send(Ok(stream.format.clone()));
            stream
        }
        Err(e) => {
            let _ = opened.send(Err(e));
            return;
        }
    };
    let Ok((mut data, on_error)) = start.recv() else {
        return;
    };
    if let Err(e) = unsafe { stream.play(&mut data, stop) } {
        on_error(e);
    }
}

/// An opened speaker, on the thread that opened it
struct Stream {
    format: Format,
    client: IAudioClient,
    render: IAudioRenderClient,
    clock: IAudioClock,
    event: HANDLE,
    buffer: u32,
}

/// Whether the mix format is 32-bit float (plain, or as WAVE_FORMAT_EXTENSIBLE)
unsafe fn is_f32(format: *const WAVEFORMATEX) -> bool {
    const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
    const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
    const SUBTYPE_IEEE_FLOAT: GUID = GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);
    let f = std::ptr::read_unaligned(format);
    if f.wBitsPerSample != 32 {
        return false;
    }
    match f.wFormatTag {
        WAVE_FORMAT_IEEE_FLOAT => true,
        WAVE_FORMAT_EXTENSIBLE => {
            let ext = std::ptr::read_unaligned(format as *const WAVEFORMATEXTENSIBLE);
            let sub = ext.SubFormat;
            sub == SUBTYPE_IEEE_FLOAT
        }
        _ => false,
    }
}

impl Stream {
    unsafe fn open(pattern: &str, cleanup: &mut Cleanup) -> Result<Stream> {
        let dev = devices::find_render_device(pattern).ok_or_else(|| {
            anyhow!(
                "Playback device \"{pattern}\" not found. Make sure the speaker is connected via \
                 USB, or choose another one in the tray menu."
            )
        })?;
        let name = devices::friendly_name(&dev).unwrap_or_default();
        let client3: IAudioClient3 = dev.Activate(CLSCTX_ALL, None)?;
        let mix = client3.GetMixFormat()?;
        cleanup.mix_format = Some(mix);
        if !is_f32(mix) {
            bail!("\"{name}\" does not use 32-bit float samples, which is not supported yet");
        }
        let f = std::ptr::read_unaligned(mix);
        let flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK;

        let (mut default, mut step, mut period, mut max) = (0, 0, 0, 0);
        client3.GetSharedModeEnginePeriod(mix, &mut default, &mut step, &mut period, &mut max)?;
        client3
            .InitializeSharedAudioStream(flags, period, mix, None)
            .with_context(|| {
                format!(
                    "Failed to open \"{name}\" at its shortest period. Is another program \
                     playing on it directly?"
                )
            })?;
        let client: IAudioClient = client3.into();

        let event = CreateEventW(None, false, false, None)?;
        cleanup.event = Some(event);
        client.SetEventHandle(event)?;
        Ok(Stream {
            format: Format {
                name,
                rate: f.nSamplesPerSec,
                channels: f.nChannels as usize,
                period,
            },
            buffer: client.GetBufferSize()?,
            render: client.GetService()?,
            clock: client.GetService()?,
            client,
            event,
        })
    }

    /// Hand `data`'s sound to the speaker whenever it asks, until `stop` is set
    unsafe fn play(&self, data: &mut Data, stop: &AtomicBool) -> Result<()> {
        let mut task = 0;
        let mmcss = AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task);
        let result = self.feed(data, stop);
        if let Ok(handle) = mmcss {
            let _ = AvRevertMmThreadCharacteristics(handle);
        }
        let _ = self.client.Stop();
        result
    }

    unsafe fn feed(&self, data: &mut Data, stop: &AtomicBool) -> Result<()> {
        let rate = self.format.rate as f64;
        let channels = self.format.channels;
        let freq = self.clock.GetFrequency()? as f64;
        let stream_latency = self.client.GetStreamLatency()? as f64 * 1e-7;
        // Frames handed to the speaker so far
        let mut written = 0u64;
        let mut waited = 0;
        self.client.Start()?;
        while !stop.load(Ordering::Relaxed) {
            if WaitForSingleObject(self.event, WAIT_MS) != WAIT_OBJECT_0 {
                waited += WAIT_MS;
                if waited >= STALL_MS {
                    bail!("The speaker stopped asking for sound");
                }
                continue;
            }
            waited = 0;
            let padding = self.client.GetCurrentPadding()?;
            let frames = self.buffer - padding;
            if frames == 0 {
                continue;
            }
            // As cpal works it out: what was handed over but not yet played, by the speaker's
            // position, decides when the next frame plays
            let (mut position, mut qpc) = (0u64, 0u64);
            self.clock.GetPosition(&mut position, Some(&mut qpc))?;
            let queued = (written as f64 / rate - position as f64 / freq).max(0.0);
            let out_lat = queued + stream_latency;
            let timing = Timing {
                playback: qpc + (out_lat * 1e7) as u64,
                out_lat,
                underrun: written > 0 && padding == 0,
            };
            let buffer = self.render.GetBuffer(frames)? as *mut f32;
            let block = std::slice::from_raw_parts_mut(buffer, frames as usize * channels);
            data(block, &timing);
            self.render.ReleaseBuffer(frames, 0)?;
            written += frames as u64;
        }
        Ok(())
    }
}
