# vmdesk

Low-latency remote desktop for a headless Oracle Cloud VM (Ubuntu 24.04, aarch64, no GPU) with a
native client for Windows and Debian laptops.

```
 VM (Ubuntu 24.04 aarch64)                                    laptop (Windows / Debian 13)
 ┌──────────────────────────────────────────────┐             ┌────────────────────────────────┐
 │ Xorg + XFCE on the vkms virtual display      │             │ client                         │
 │   └─ kernel framebuffer (DRM GETFB2 + mmap)  │   WebRTC    │   H.264 → FFmpeg (D3D11VA /    │
 │ server: XRGB→I420 → OpenH264 → RTP/SRTP  ────┼── UDP ─────►│   VA-API, sw fallback) → window│
 │         uinput keyboard + absolute pointer ◄─┼── data ch. ─┤   keys / mouse / wheel          │
 │ signalling: HTTP 127.0.0.1:8080 (POST /offer)│◄─ ssh -L ───┤                                │
 └──────────────────────────────────────────────┘             └────────────────────────────────┘
```

* Capture: the server reads the vkms primary-plane framebuffer straight from the kernel (no X
  extensions, no screenshots), 30 fps by default.
* Encoding: OpenH264 (bundled source, no FFmpeg on the server), 12 Mbit/s default, High
  profile, keyframes on demand.
* Transport: WebRTC (ICE-lite server, UDP 50000-50100 over IPv4 and IPv6, one video track,
  `control` + `mouse` data channels). ICE picks whichever path works; the title bar shows it.
  Signalling is plain HTTP on the VM's loopback, reached through an SSH tunnel.
* Input: uinput devices on the VM; the client sends physical key codes, the VM's keyboard
  layout applies.
* Not in v1: audio, clipboard, remote cursor shape, TLS, multiple clients.

## 1. Get the binaries

Every push to `main` builds a GitHub Release (see `.github/workflows/release.yml`) with three
files; a manual run of the workflow on another branch publishes the same files as a pre-release:

| file                 | runs on                                                       |
|----------------------|---------------------------------------------------------------|
| `server`             | the VM (aarch64 Ubuntu 24.04)                                  |
| `client-windows.zip` | Windows x86_64: `client.exe` plus the FFmpeg DLLs it needs     |
| `client`             | Debian 13 x86_64, uses the distro's FFmpeg libraries (below)   |

Download them from the repository's *Releases* page.

## 2. Install the server on the VM

From your laptop (replace `<vm-ip>` with the VM's public IP):

```sh
scp server ubuntu@<vm-ip>:~/
ssh ubuntu@<vm-ip>
chmod +x server
sudo ./server setup --public-ip <vm-public-ipv4>
```

`--public-ip` is the VM's public IPv4 as shown in the OCI console. It is only needed when the
OCI metadata service does not report a `publicIp` for the VNIC (setup prints a warning in that
case); the value is stored in the config and kept by later runs.

`setup` installs Xorg/XFCE/lightdm, configures Xorg on the vkms display (1920x1080), autologin
for `ubuntu`, the udev rule for `/dev/uinput`, the `iptables` and `ip6tables` rules for UDP
50000-50100 (persisted), `/etc/vmdesk/config.toml`, and the `vmdesk` systemd service that runs
`/usr/local/bin/vmdesk-server run`. It ends with a short checklist. Then:

```sh
sudo reboot
```

### OCI security list

The VM's own firewall is handled by `setup`, but Oracle's network still blocks the ports. In
the OCI console: **Networking → Virtual cloud networks → your VCN → Subnets → your subnet →
Security Lists → Default Security List → Add Ingress Rules**:

* Source CIDR: your laptop's public IP as `/32` (or `0.0.0.0/0` if it changes often)
* IP Protocol: **UDP**
* Destination Port Range: **50000-50100**

Add a second rule for IPv6 (the VM's global IPv6 address is offered as an alternative media
path): Source CIDR `::/0` (or your laptop's IPv6 prefix), UDP, ports 50000-50100.

(If the instance uses a Network Security Group instead, add the same rules there.) Port 8080 is
**not** opened anywhere; signalling always goes through SSH.

### Check the VM

After the reboot:

```sh
ssh ubuntu@<vm-ip>
./server doctor
```

Every line is `PASS`, `WARN` or `FAIL` with the fix printed underneath. See
[Troubleshooting](#troubleshooting). Useful commands:

```sh
sudo systemctl status vmdesk        # service state
journalctl -u vmdesk -f             # live server log
./server capture --png /tmp/a.png   # grab one frame from the framebuffer (needs CAP_SYS_ADMIN,
                                    # which setup granted to ./server and the installed copy)
```

While a client is connected the log prints two `pipeline:` lines every 5 seconds:

```
pipeline: captured 30.0 fps, encoded 30.0 fps, sent 30.0 fps, 11800 kbit/s (target 12000), 1 IDR, dropped 0 (encoder busy) + 0 (sink full)
pipeline timings avg/max ms: capture+copy 2.1/3.9, convert 1.1/2.0, encode 9.7/24.0, packetize+send 0.4/1.8
```

Every frame is encoded (a static desktop costs a few hundred bytes per frame). The three
rates should all equal `video.fps`; if `encoded` is lower, the `encode` time is above the
frame time (33 ms at 30 fps): lower `video.fps` or `bitrate_kbps`, or check the VM's CPU load.

## 3. Run the client

### SSH tunnel (both laptops)

Keep this running while you use the desktop:

```sh
ssh -N -L 8080:127.0.0.1:8080 ubuntu@<vm-ip>
```

On Windows use the built-in OpenSSH (`ssh` in PowerShell) or PuTTY (Connection → SSH →
Tunnels: source port `8080`, destination `127.0.0.1:8080`).

### Windows

Unzip `client-windows.zip` anywhere and start `client.exe` (double-click, or from PowerShell
to pass flags). The executable is not code-signed, so SmartScreen shows "Windows protected your
PC" the first time: *More info → Run anyway*. Hardware decoding uses D3D11VA on the Intel GPU;
the window title shows `h264 (d3d11va)` when it is active.

### Debian 13

Install the runtime libraries once:

```sh
sudo apt install libavcodec61 libavutil59 libswscale8 \
                 libva2 libva-drm2 libva-x11-2 intel-media-va-driver vainfo
```

(`sudo apt install ffmpeg` pulls the same libav* packages if you prefer.) For Intel GPUs older
than Broadwell (2014) install `i965-va-driver` instead of `intel-media-va-driver`. Then:

```sh
chmod +x client
./client
```

`vainfo` should list `VAProfileH264High : VAEntrypointVLD`; if it does, the title shows
`h264 (vaapi)`.

### Flags and controls

```
client [--server http://127.0.0.1:8080] [--bitrate <kbit/s>] [--no-hwdec] [-v]
```

* `--server`: signalling URL, default is the tunnel above.
* `--bitrate`: request a different bitrate for this session (server default 12000 kbit/s).
  Lower it (e.g. `--bitrate 6000`) on slow links.
* `--no-hwdec`: force FFmpeg's software h264 decoder.
* **F11** toggles fullscreen. The title bar shows the connection state including the selected
  media path (`connected via IPv6 to [2603:...]:50000` or `via IPv4 to 82.70.62.40:50000`),
  the active decoder, the video size and the decoded frame rate. The server logs the same pair
  (`media path IPv6: local ... <-> remote ...`).
* Keys are sent as physical positions: the layout configured in XFCE on the VM decides what
  they produce (*Settings → Keyboard → Layout*). Some combinations are taken by the local OS
  (e.g. the Windows key, Alt+Tab on some desktops) and never reach the client window.
* Closing the window or losing focus releases every key and button on the VM.

### Updating the server

Copy the new `server` binary to the VM and re-run setup without the package installation; it
re-installs the binary, re-applies the capability, rewrites the config while keeping the values
already in it (`public_ip` included) and restarts the service:

```sh
scp server ubuntu@<vm-ip>:~/ && ssh ubuntu@<vm-ip> 'chmod +x server && sudo ./server setup --skip-apt'
```

To change the public address later: `sudo ./server setup --skip-apt --public-ip <ipv4>`
(and `--public-ipv6 <ipv6>` to override the interface's IPv6 address).

## 4. Configuration

`/etc/vmdesk/config.toml` (written by `setup`, all keys optional). After editing:
`sudo systemctl restart vmdesk`.

```toml
[video]
fps = 30                    # capture/encode rate
bitrate_kbps = 12000        # H.264 target bitrate (client --bitrate overrides per session)
keyframe_interval_secs = 10 # IDR + SPS/PPS at least every N seconds; 0 = only on connect/PLI
keyframe_interval = 0       # extra encoder-internal keyframe interval in frames; 0 = none
encoder_threads = 0         # OpenH264 threads, 0 = auto (max 4)

[network]
public_ip = ""           # public IPv4; "" = read from the OCI metadata service
metadata_url = "http://169.254.169.254/opc/v2/vnics/"
ipv6 = true              # also offer the VM's global IPv6 address as a media path
public_ipv6 = ""         # IPv6 to advertise instead of the interface's own; "" = automatic
udp_port_min = 50000     # WebRTC UDP range (one port per connection)
udp_port_max = 50100
signalling_addr = "127.0.0.1:8080"

[capture]
card = ""                # DRM node of the vkms device, "" = autodetect by driver name
connector = ""           # "" = the vkms card's Virtual-* connector, whatever its index
```

## Troubleshooting

Run `./server doctor` on the VM and find the failing line here.

| doctor line | meaning / fix |
|---|---|
| `FAIL vkms module` | The virtual display driver is not loaded: `sudo modprobe vkms`. `setup` persists it in `/etc/modules-load.d/vmdesk.conf`; if it still fails after a reboot check `dmesg | grep vkms`. |
| `FAIL vkms card` | No `/dev/dri/card*` belongs to driver `vkms`. Load the module (above). If the VM has another DRM device, set `capture.card` in the config to the vkms node (`/dev/dri/by-path/platform-vkms-card`). |
| `FAIL Xorg on vkms` | Xorg is not driving the vkms connector (`Virtual-N`; the index depends on which DRM devices the kernel found first, so leave `capture.connector` empty and the server picks the vkms card's Virtual connector). `systemctl status lightdm`, `journalctl -u lightdm -b`, `grep -E '\(EE\)|vkms' /var/log/Xorg.0.log`. Typical causes: no reboot after `setup`, lightdm not enabled, wrong `kmsdev` path in `/etc/X11/xorg.conf.d/10-vkms.conf`. |
| `FAIL framebuffer capture` | The display is up but the framebuffer cannot be read. The kernel only hands out buffer handles to `CAP_SYS_ADMIN`: `sudo setcap cap_sys_admin+ep ./server` (the service gets the capability from its unit file). |
| `FAIL CAP_SYS_ADMIN` | Same fix as above for the binary you are running by hand. |
| `FAIL /dev/uinput` | Missing: `sudo modprobe uinput`. Not writable: the udev rule `/etc/udev/rules.d/70-vmdesk-uinput.rules` or the `input` group membership is missing; re-run `sudo ./server setup`, then log out and in. Quick fix: `sudo chgrp input /dev/uinput && sudo chmod 660 /dev/uinput`. Without it the server still streams video but ignores input (the log says `input disabled`). |
| `WARN groups` | `sudo usermod -aG input,video ubuntu`, then log out and in (the service is unaffected: its unit sets `SupplementaryGroups`). |
| `WARN xorg.conf.d` | `setup` was not run on this machine. |
| `FAIL public IPv4` | The OCI metadata service did not report a public IP (some VNIC configurations omit `publicIp`). Run `sudo ./server setup --skip-apt --public-ip <VM public IPv4>`; it stores the address in the config and restarts the service. |
| `WARN IPv6` | No global IPv6 address on the VM: only the IPv4 path is offered. Fine if the VNIC has no IPv6; otherwise assign one in the OCI console (or set `ipv6 = false` to silence the warning). |
| `FAIL iptables` | No ACCEPT rule for UDP 50000-50100, or it sits below OCI's default `REJECT` rule. `sudo iptables -I INPUT 1 -p udp -m udp --dport 50000:50100 -j ACCEPT && sudo netfilter-persistent save`. |
| `FAIL ip6tables` | Same for IPv6: `sudo ip6tables -I INPUT 1 -p udp -m udp --dport 50000:50100 -j ACCEPT && sudo netfilter-persistent save`. |
| `FAIL signalling port` | Something else listens on 127.0.0.1:8080. Stop it or change `network.signalling_addr` and the `ssh -L` port. |
| `WARN systemd service` | `sudo systemctl enable --now vmdesk`; errors: `journalctl -u vmdesk -b`. |
| `WARN frame conversion` | Copying and converting a 1080p frame takes more than 12 ms (normally 2-4 ms with the NEON kernel on the Ampere cores). The VM is CPU starved: check `top` for other load. |
| `WARN encoder` / `FAIL encoder` | OpenH264 encodes a second of moving 1080p video slower than `video.fps` (the line shows the sustained fps, ms per frame and the keyframe size). Lower `video.fps` or `bitrate_kbps`, set `encoder_threads = 4`, and check the CPU load. `FAIL` with an error message: OpenH264 could not initialise, please open an issue. |

Client-side symptoms:

| symptom | fix |
|---|---|
| `POST http://127.0.0.1:8080/offer failed` | The SSH tunnel is not running, or the service is down (`systemctl status vmdesk` on the VM). |
| `server rejected the offer (500)` | Read `journalctl -u vmdesk` on the VM; usually the UDP port could not be bound or the display is not active yet. |
| Title stuck at `connecting (ICE)`, then `could not connect: ICE failed` | UDP does not get through on either family: OCI security list rules missing (IPv4 and IPv6), `FAIL iptables`/`ip6tables`, or `FAIL public IPv4` (the answer then advertises the VM's private IPv4). Check `doctor`, and that your laptop's network allows outbound UDP to ports 50000-50100. The server log prints the advertised host candidates for every offer. |
| Connected via IPv4 although both ends have IPv6 | ICE nominated the first pair that answered. Both paths work; to prefer IPv6 block the IPv4 rule temporarily or check that the OCI IPv6 ingress rule exists (`connected via IPv6 ...` then shows in the title). |
| Connected but the window stays black | No decodable keyframe has arrived. The client asks for one (PLI) at most once a second until the first picture decodes, and the server sends an IDR with SPS/PPS on every request and every `keyframe_interval_secs`. Check the server log: the `pipeline:` lines must show `encoded` at the configured fps and `sent` equal to it; `encode failed` or capture errors point at the VM side. Run the client with `-v` to see `requesting a keyframe` and the decoder's reasons; `--no-hwdec` rules out the hardware decoder. |
| Log lines `srtp ssrc=... index=N: duplicated` (client) or `srtcp ...: duplicated` (server) | A retransmitted or reordered packet arrived outside the replay window and was rejected. With the current windows (4096 packets on the client, 1024 on the server) this only happens on badly reordering paths; a burst of them is summarised by the logger (`N more warn message(s) ... suppressed`). Video recovers by itself through NACK retransmissions and, failing that, a PLI. |
| `video: N access units, M dropped (packet loss), K late/duplicate packets` (client log) | Reassembly statistics, printed every 10 s only when something was lost. `dropped` frames were waited for 500 ms for a retransmission and then skipped; the decoder then requests a keyframe. Persistent loss: lower `--bitrate`. |
| Title shows `h264 (software)` on the Debian laptop | VA-API is unavailable: install `intel-media-va-driver` (or `i965-va-driver`), check `vainfo`. Software decoding of 1080p30 still works on any recent laptop, just with more CPU use. |
| Title shows `h264 (software, d3d11va rejected the stream)` | The Windows GPU driver refused the stream; update the Intel graphics driver. Software decoding continues. |
| Smearing / blockiness after a moment | Packet loss; the client requests keyframes automatically. Lower the bitrate: `--bitrate 6000`. |
| Wheel or extra mouse buttons do the wrong thing | Open an issue with the mouse model; the mapping lives in `client/src/app.rs` and `server/src/input/keymap.rs`. |

## Building from source

Rust stable (see `rust-toolchain.toml`).

* Tests and lints (any Linux host with `libavcodec-dev libavutil-dev libswscale-dev clang`):
  `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings`
* End-to-end tests (`cargo test -p e2e`, part of the workspace run) link the server and client
  libraries: `media_path` encodes synthetic frames with OpenH264, packetizes them to RTP,
  reassembles and decodes them with FFmpeg's software decoder (also with the SPS/PPS packet
  lost, checking the keyframe request); `webrtc_loopback` connects the real server and client
  WebRTC stacks over UDP on the host's interface, streams 90 frames of 720p and asserts that
  every access unit arrives exactly once and decodes, with and without 4% packet loss (NACK
  recovery). The loopback tests need a non-loopback network interface and UDP ports 50777-50778.
* aarch64 only: the NEON XRGB→I420 kernel is compared bit for bit against the scalar
  reference by `convert::tests::platform_kernel_matches_scalar_reference` (the ARM CI job).
* Server for the VM: CI builds it natively on GitHub's `ubuntu-24.04-arm` runner
  (`cargo build --release -p server`). From an x86_64 machine cross-compile with
  `pip install cargo-zigbuild ziglang` and
  `cargo zigbuild --release -p server --target aarch64-unknown-linux-gnu.2.39`
  (`target/aarch64-unknown-linux-gnu/release/server`).
* Linux client: `cargo build --release -p client` on Debian 13 (or in a `debian:13` container,
  which is what CI does so the binary matches the laptop's FFmpeg 7.1).
* Windows client: install LLVM (for bindgen), download a shared FFmpeg build (CI uses BtbN's
  `ffmpeg-n8.1-latest-win64-lgpl-shared-8.1`), set `FFMPEG_DIR` to the extracted folder and
  `LIBCLANG_PATH` to LLVM's `bin`, then `cargo build --release -p client`. Ship the DLLs from
  `FFMPEG_DIR\bin` next to `client.exe`.

Repository layout: `proto/` (wire protocol, key codes, SDP helpers, logging), `server/`
(capture, convert, pipeline, encoder, session, signalling, input, setup, doctor; a library plus
the `server` binary), `client/` (net, assembler, decoder, scaler, app, keymap; a library plus
the `client` binary), `e2e/` (end-to-end tests).
