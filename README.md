# Stereo Split

Splits your PC's audio into its left and right channels and sends each one to a separate USB speaker, while the keyboard volume keys keep controlling both. Written for building a stereo pair out of two single-driver speakers (such as the Xiaomi Smart Speaker Pro in USB mode).

## What it does

1. Uses loopback capture to read what CABLE Input is playing. It never opens a recording device, so Windows won't show "microphone in use", and your microphone isn't affected.
2. Sends the left channel to the left speaker and the right channel to the right speaker, fully separated with no bleed between them.
3. Reads the volume and mute state you set for CABLE Input in Windows and applies them to both speakers in real time, so the keyboard volume keys and the taskbar volume slider work as usual.
4. Resamples for each speaker on its own, so the speakers and CABLE Input can use different sample rates.
5. Corrects clock drift between the two speakers automatically by adjusting each speaker's playback speed very slightly, so left and right stay in sync even during long playback.
6. Makes CABLE Input the default playback device while it runs, and switches back to your speaker when it exits. If it crashes, Windows restarts it automatically.
7. Reconnects automatically when a speaker is unplugged and plugged back in, lives in the system tray, and can start with Windows.

## First-time setup

### 1. Install VB-CABLE

Download and install it from <https://vb-audio.com/Cable> (free donationware), then restart your PC.

### 2. Adjust a few Windows settings (optional)

Open "Control Panel → Sound":

- Tip: double-click each of the two speakers and, on the "General" tab, rename them to `Speaker-Left` and `Speaker-Right` (or any names you can easily tell apart), so they're easy to pick in the tray menu.
- Reset "Levels → Balance" on both speakers to equal left/right if you changed it before. This program splits the channels itself, so balance is no longer needed.
- On the "Recording" tab, check that your microphone is still the default device (Windows sometimes makes CABLE Output the default when VB-CABLE is installed).

You don't need to change the default playback device or match sample rates; the program takes care of both.

### 3. Run the program and choose your speakers

Put `stereo-split.exe` in a permanent folder (for example `D:\Tools\StereoSplit\`) and double-click it.

Right-click the tray icon (a dot that's blue on the left and orange on the right), pick your speakers under "Left speaker" and "Right speaker", then use "Test left" / "Test right" to check that each one beeps from the side you expect. If they're the wrong way round, choose "Swap left / right".

Every change takes effect immediately; there's nothing to save or reload.

### 4. Start with Windows

Right-click the tray icon and check "Start with Windows".

## Tray menu

| Item                         | What it does                                                               |
| ---------------------------- | -------------------------------------------------------------------------- |
| Status                       | Shows Running, Choose speakers, Reconnecting, Config error, etc.           |
| Left speaker / Right speaker | Pick the speaker for each channel. The list refreshes when you plug one in |
| Swap left / right            | Swaps the two speakers                                                     |
| Test left / Test right       | Plays a short beep on that speaker                                         |
| Latency                      | Buffer size. Raise it if you hear crackling, lower it for less delay       |
| Start with Windows           | Adds or removes the startup entry for the current user                     |
| Open config file             | Opens `config.toml` for the advanced settings. Saving applies them         |
| Open log folder              | One log file per run, newest last. Look here when something goes wrong     |
| Exit                         | Exits the program and switches the default device back to your speaker     |

## Troubleshooting

**No sound at all**: Check "Status" in the tray menu and the newest log file. If it says "Choose speakers", pick both speakers in the tray menu.

**Crackling or dropouts**: Set "Latency" in the tray menu to 50 ms. While it crackles, the log gets an `audio dropouts` line every 10 seconds, showing which speaker ran dry and how late the sound arrived.

**Left and right are swapped**: Choose "Swap left / right" in the tray menu.

**Volume keys do nothing**: Make sure `volume_endpoint` in `config.toml` matches CABLE Input.

**The tray shows "microphone in use"**: Open `config.toml` and make sure `source = "CABLE Input"` (older versions defaulted to CABLE Output).

**Going back to normal**: Just exit the program; the default playback device switches back to the speaker you used before.

**No sound after ending it from Task Manager**: Ending the program that way skips switching back. Start it again and choose "Exit", or pick your speaker in the taskbar's sound menu.

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
