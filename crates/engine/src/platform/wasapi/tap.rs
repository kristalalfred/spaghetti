//! Claude: an app's audio, captured through process loopback: the audio
//! engine's mix of every stream a process and the processes under it play.
//! The capture follows the app's session volume and mute in the system's
//! volume mixer, so muting the app there silences the tap too, and a tap
//! that mutes the app is refused.

use std::collections::BTreeMap;
use std::mem::ManuallyDrop;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use spaghetti_common::ipc_protocol::{AppInfo, FlowStatus, TapMode};
use windows::Win32::Media::Audio::{
    AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
    AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
    ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
    IAudioClient, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
    VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
};
use windows::Win32::System::Com::BLOB;
use windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
};
use windows::Win32::System::Variant::VT_BLOB;
use windows::core::{HRESULT, IUnknown, Interface, Ref, implement};

use super::capture::{Capture, Dating, Opening, Shared};
use super::processes::{self, Snapshot};
use super::sessions;
use crate::flow::Link;
use crate::ring::Ring;
use crate::sinks::SourceTiming;
use crate::spec::Tapped;

/// Claude: process loopback converts to any format asked of it, so a tap
/// runs at one rate whatever the devices do.
const TAP_RATE: u32 = 48000;
const ACTIVATION_LIMIT: Duration = Duration::from_secs(5);

/// Claude: an app's audio. Its capture runs only while at least one route
/// reads the frames.
pub struct TapSource {
    shared: Arc<Shared>,
    capture: Option<Capture>,
    root: u32,
    pids: Vec<u32>,
    pub label: String,
}

/// Claude: the one process whose tree a tap captures. Process loopback takes
/// a single process and the processes under it.
fn tree_root(pids: &[u32]) -> Result<u32, String> {
    let snapshot = Snapshot::take();
    if let Some(pid) = pids.iter().find(|&&pid| !snapshot.contains(pid)) {
        return Err(format!("process {pid} is not running"));
    }
    match snapshot.roots(pids)[..] {
        [] => Err("a tap needs at least one process".into()),
        [root] => Ok(root),
        ref roots => Err(format!(
            "a tap captures one process and those under it; these processes form {} trees",
            roots.len()
        )),
    }
}

impl TapSource {
    pub fn create(_source_id: u32, tapped: Tapped, mode: TapMode) -> Result<Self, String> {
        let Tapped::Processes(pids) = tapped else {
            return Err(
                "Windows captures one process and those under it, not every process but some"
                    .into(),
            );
        };
        if mode == TapMode::MuteSourceWhenTapped {
            return Err(
                "Windows silences an app's capture along with its output, so a tap cannot mute the app"
                    .into(),
            );
        }
        let root = tree_root(&pids)?;
        Ok(Self {
            shared: Arc::new(Shared::new()),
            capture: None,
            root,
            pids,
            label: app_label(root),
        })
    }

    pub fn status(&self) -> FlowStatus {
        let link = if self.capture.is_some() {
            Link::Connected {
                peer: None,
                packets_lost: 0,
                packets_retransmitted: 0,
            }
        } else {
            Link::Idle
        };
        self.shared.meter.status(link)
    }

    /// Claude: a running capture keeps running while the processes stay
    /// under the same root; a new root restarts it.
    pub fn set_processes(&mut self, pids: &[u32]) -> Result<(), String> {
        let root = tree_root(pids)?;
        self.pids = pids.to_vec();
        if root != self.root {
            self.root = root;
            if self.capture.is_some() {
                self.capture = None;
                self.capture = Some(self.start()?);
            }
        }
        tracing::info!(label = self.label, ?pids, "updated tap processes");
        Ok(())
    }

    pub fn timing(&mut self) -> Result<SourceTiming, String> {
        Ok(SourceTiming {
            sample_rate: f64::from(TAP_RATE),
            delivery: Arc::clone(&self.shared.delivery),
        })
    }

    pub fn add_sink(&mut self, ring: Arc<Ring>) -> Result<usize, String> {
        let slot = self.shared.attach(ring)?;
        if self.capture.is_none() {
            match self.start() {
                Ok(capture) => self.capture = Some(capture),
                Err(e) => {
                    self.shared.detach(slot);
                    return Err(e);
                }
            }
        }
        Ok(slot)
    }

    pub fn remove_sink(&mut self, slot: usize) {
        self.shared.detach(slot);
        if self.shared.is_empty() && self.capture.take().is_some() {
            tracing::info!(label = self.label, "stopped tap capture");
        }
    }

    fn start(&self) -> Result<Capture, String> {
        let root = self.root;
        let capture = Capture::start(
            &self.label,
            TAP_RATE,
            Dating::Measured,
            Arc::clone(&self.shared),
            move || {
                Ok(Opening {
                    client: activate_loopback(root)?,
                    loopback: true,
                })
            },
        )?;
        tracing::info!(label = self.label, root, "started tap capture");
        Ok(capture)
    }
}

#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Activated(mpsc::SyncSender<()>);

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(
        &self,
        _operation: Ref<'_, IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        let _ = self.0.try_send(());
        Ok(())
    }
}

/// Claude: a client on the process loopback of `pid` and every process
/// under it.
fn activate_loopback(pid: u32) -> Result<IAudioClient, String> {
    let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            },
        },
    };
    // Claude: the blob points at `params` on this stack, so the variant must
    // never be cleared, which would free it.
    let variant = ManuallyDrop::new(PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_BLOB,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: PROPVARIANT_0_0_0 {
                    blob: BLOB {
                        cbSize: size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                        pBlobData: (&raw mut params).cast(),
                    },
                },
            }),
        },
    });
    let (done, completed) = mpsc::sync_channel(1);
    let handler: IActivateAudioInterfaceCompletionHandler = Activated(done).into();
    let operation = unsafe {
        ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some((&raw const *variant).cast()),
            &handler,
        )
    }
    .map_err(|e| format!("open the process loopback: {e}"))?;
    completed
        .recv_timeout(ACTIVATION_LIMIT)
        .map_err(|_| "the process loopback did not open in time".to_string())?;
    let mut result = HRESULT(0);
    let mut client: Option<IUnknown> = None;
    unsafe { operation.GetActivateResult(&mut result, &mut client) }
        .and_then(|()| result.ok())
        .map_err(|e| format!("open the process loopback: {e}"))?;
    client
        .ok_or("the process loopback opened no client")?
        .cast()
        .map_err(|e| format!("open the process loopback: {e}"))
}

/// Claude: the processes playing through any render endpoint, grouped under
/// the app each belongs to: the outermost process of the same executable
/// they run under, which a tap of the app captures with everything under it.
pub fn list_audio_apps() -> Vec<AppInfo> {
    let snapshot = Snapshot::take();
    let mut apps: BTreeMap<u32, AppInfo> = BTreeMap::new();
    for pid in sessions::pids() {
        if !snapshot.contains(pid) {
            continue;
        }
        let root = snapshot.app_root(pid);
        let app = apps.entry(root).or_insert_with(|| {
            let path = processes::path(root);
            AppInfo {
                name: path
                    .as_deref()
                    .map_or_else(|| format!("PID {root}"), processes::display_name),
                bundle_id: None,
                bundle_path: path,
                pids: vec![root],
            }
        });
        app.pids.push(pid);
    }
    let mut apps: Vec<AppInfo> = apps.into_values().collect();
    for app in &mut apps {
        app.pids.sort_unstable();
        app.pids.dedup();
    }
    apps.sort_by_key(|app| app.name.to_lowercase());
    apps
}

fn app_label(pid: u32) -> String {
    processes::path(pid)
        .as_deref()
        .map_or_else(|| format!("PID {pid}"), processes::display_name)
}

#[cfg(test)]
mod tests {
    use std::process::{Child, Command};
    use std::thread;
    use std::time::Instant;

    use super::super::com;

    use windows::Win32::Media::Audio::{
        AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, IAudioRenderClient, eConsole,
        eRender,
    };

    use super::*;

    /// Claude: -80 dBFS, far below hearing at any volume, well above zero.
    const TONE: f32 = 1e-4;
    const TONE_ENV: &str = "SPAGHETTI_TEST_TONE_SECONDS";

    /// Claude: plays a quiet tone on the default output while the test that
    /// spawns this binary asks for it, and passes otherwise.
    #[test]
    #[ignore = "a tone for tap_a_tone_process; it spawns this"]
    fn tone_process() {
        let Some(seconds) = std::env::var(TONE_ENV)
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
        else {
            return;
        };
        com::init();
        let device = unsafe {
            com::enumerator()
                .unwrap()
                .GetDefaultAudioEndpoint(eRender, eConsole)
        }
        .unwrap();
        let client = com::activate(&device).unwrap();
        let format = com::stereo_float(48000);
        unsafe {
            client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
                    1_000_000,
                    0,
                    &format,
                    None,
                )
                .unwrap();
            let render: IAudioRenderClient = client.GetService().unwrap();
            let size = client.GetBufferSize().unwrap();
            client.Start().unwrap();
            let end = Instant::now() + Duration::from_secs(seconds);
            let mut phase = 0u64;
            while Instant::now() < end {
                let free = size - client.GetCurrentPadding().unwrap();
                if free > 0 {
                    let data = render.GetBuffer(free).unwrap().cast::<f32>();
                    for f in 0..free as usize {
                        let s =
                            TONE * (phase as f32 * 440.0 * std::f32::consts::TAU / 48000.0).sin();
                        *data.add(2 * f) = s;
                        *data.add(2 * f + 1) = s;
                        phase += 1;
                    }
                    render.ReleaseBuffer(free, 0).unwrap();
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
    }

    struct Tone(Child);

    impl Drop for Tone {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn has_session(pid: u32) -> bool {
        com::init();
        sessions::pids().contains(&pid)
    }

    fn peak(ring: &Ring, seconds: f64) -> (usize, f32) {
        let end = Instant::now() + Duration::from_secs_f64(seconds);
        let mut buffer = vec![0.0; 4096];
        let (mut frames, mut peak) = (0, 0.0f32);
        while Instant::now() < end {
            ring.take_discontinuity();
            let read = ring.pop_into(&mut buffer);
            frames += read;
            peak = buffer[..read * 2].iter().fold(peak, |p, s| p.max(s.abs()));
            thread::sleep(Duration::from_millis(20));
        }
        (frames, peak)
    }

    /// Claude: the tap hears the app as it plays, and the app is listed
    /// under its own process.
    #[test]
    #[ignore = "plays a -80 dBFS tone on the default output"]
    fn tap_a_tone_process() {
        let tone = Tone(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "platform::wasapi::tap::tests::tone_process",
                    "--ignored",
                ])
                .env(TONE_ENV, "20")
                .spawn()
                .unwrap(),
        );
        let pid = tone.0.id();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !has_session(pid) {
            assert!(
                Instant::now() < deadline,
                "the tone process opened no session"
            );
            thread::sleep(Duration::from_millis(100));
        }
        assert!(list_audio_apps().iter().any(|app| app.pids.contains(&pid)));

        let mut tap = TapSource::create(1, Tapped::Processes(vec![pid]), TapMode::Mirror).unwrap();
        let ring = Arc::new(Ring::new(f64::from(TAP_RATE)));
        let slot = tap.add_sink(Arc::clone(&ring)).unwrap();
        let (frames, level) = peak(&ring, 1.5);
        assert!(frames > 48000, "{frames} frames");
        assert!((level - TONE).abs() < TONE * 0.05, "peak {level}");
        tap.remove_sink(slot);
    }

    #[test]
    fn a_tap_that_mutes_its_app_is_refused() {
        let refused = TapSource::create(
            1,
            Tapped::Processes(vec![std::process::id()]),
            TapMode::MuteSourceWhenTapped,
        );
        assert!(refused.is_err());
    }
}
