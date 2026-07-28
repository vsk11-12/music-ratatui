# 🎵 tuiplay

A lightweight, modern terminal music player written in Rust. Built with **Ratatui** for UI, **lofty** for metadata extraction, and powered by an **`mpv` IPC socket backend** for rock-solid audio decoding across all file formats.

Designed specifically as an offline, single-purpose CLI player with Vim-style navigation, queue management, album artwork rendering, and native media key support.

---

## ✨ Features

- ⚡ **IPC Audio Engine:** Uses `mpv` as a headless background process for playback. Handles FLAC, WAV, MP3, M4A, ALAC, AAC, OPUS, OGG, WMA, and AIFF effortlessly without crashing.
- 🎨 **Zen Mode & Album Art:** Displays embedded album artwork using terminal graphics protocols via `ratatui-image`.
- 🧭 **Vim-style Directory Browsing:** Fast directory traversal with instantaneous inline search (`/`).
- 📜 **Queue Management:** Easily queue tracks, remove items, and reorder tracks on the fly.
- ⌨️ **Media Key & MPRIS Integration:** Native MPRIS signal listening via `souvlaki` for Linux desktop integration, system media keys, and Bluetooth controls.
- 💾 **State Persistence:** Automatically saves your volume, current directory, active view, and queue to `~/.config/tuiplay/state.json` between sessions.
- 🎨 **Custom Theme Support:** Configurable colors via a `theme.json` file.

---

## 📋 Prerequisites

`tuiplay` requires `mpv` to be installed on your system to handle audio playback.

### Arch Linux
```bash
sudo pacman -S mpv
