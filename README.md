# Stereo Split

Splits your PC's audio into its left and right channels and sends each one to a separate USB speaker, while the keyboard volume keys keep controlling both. Written for building a stereo pair out of two single-driver speakers (such as the Xiaomi Smart Speaker Pro in USB mode).

## What it does

1. Uses loopback capture to read what CABLE Input is playing. It never opens a recording device, so Windows won't show "microphone in use", and your microphone isn't affected.
2. Sends the left channel to the left speaker and the right channel to the right speaker, fully separated with no bleed between them.
3. Reads the volume and mute state you set for CABLE Input in Windows and applies them to both speakers in real time, so the keyboard volume keys and the taskbar volume slider work as usual.
4. Corrects clock drift between the two speakers automatically, so left and right stay in sync even during long playback (to within about 1 ms).
5. Reconnects automatically when a speaker is unplugged and plugged back in, lives in the system tray, and can start with Windows.

## First-time setup

### 1. Install VB-CABLE

Download and install it from <https://vb-audio.com/Cable> (free donationware), then restart your PC.

### 2. Adjust a few Windows settings

Open "Control Panel → Sound":

- On the "Playback" tab, set **CABLE Input** as the default device.
- Double-click each of the two speakers and, on the "General" tab, rename them to `Speaker-Left` and `Speaker-Right` (or any names you can easily tell apart).
- Reset "Levels → Balance" on both speakers to equal left/right if you changed it before. This program splits the channels itself, so balance is no longer needed.
- **Sample rates must match**: for both speakers and CABLE Input (all on the Playback tab), choose the same sample rate under "Properties → Advanced → Default Format". `48000 Hz` is recommended.
- On the "Recording" tab, check that your microphone is still the default device (Windows sometimes makes CABLE Output the default when VB-CABLE is installed).

### 3. Run the program

Put `stereo-split.exe` in a permanent folder (for example `D:\Tools\StereoSplit\`) and double-click it.

On first run it creates `config.toml` in the same folder and shows a notice. Right-click the tray icon (a dot that's blue on the left and orange on the right) → "Open config file", change `left` and `right` to your speakers' names (part of the name is enough), save, then right-click → "Reload config".

`devices.txt` in the same folder lists the full names of every audio device on your PC, in case you're unsure.

### 4. Start with Windows

Right-click the tray icon and check "Start with Windows".

## Tray menu

| Item               | What it does                                           |
| ------------------ | ------------------------------------------------------ |
| Status             | Shows Running, Reconnecting, Config error, etc.        |
| Open config file   | Opens `config.toml` in Notepad                         |
| Reload config      | Applies config changes immediately, no restart needed  |
| View log           | Look here when something goes wrong                    |
| Start with Windows | Adds or removes the startup entry for the current user |
| Exit               | Exits the program                                      |

## Troubleshooting

**No sound at all**: Make sure the default playback device is CABLE Input, then check "Status" in the tray menu and the log.

**A popup says the sample rates don't match**: Set every device to the same sample rate as described in step 2 above.

**Crackling or dropouts**: Raise `latency_ms` in `config.toml` to 50.

**Left and right are swapped**: Swap the values of `left` and `right` in the config.

**Volume keys do nothing**: Make sure `follow_windows_volume = true` and that `volume_endpoint` matches CABLE Input.

**Volume changes are too steep (one step drops it a lot)**: Windows is already applying the volume to the loopback audio, and the program applies it a second time. Set `follow_windows_volume` to `false`.

**The tray shows "microphone in use"**: Open `config.toml` and make sure `source = "CABLE Input"` (older versions defaulted to CABLE Output).

**Going back to normal**: Exit the program and set the default playback device back to one of the speakers.

## Building from source

Requires the Rust toolchain. On Windows:

```sh
cargo build --release
```

To cross-compile on Linux:

```sh
rustup target add x86_64-pc-windows-gnu
sudo apt install gcc-mingw-w64-x86-64
cargo build --release --target x86_64-pc-windows-gnu
```
