# OxiPlay 🦀🎬

[![CI](https://github.com/micferna/oxiplay/actions/workflows/ci.yml/badge.svg)](https://github.com/micferna/oxiplay/actions/workflows/ci.yml)
[![Licence: GPL-3.0](https://img.shields.io/badge/licence-GPL--3.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-stable%20%E2%89%A5%201.80-orange.svg)](https://www.rust-lang.org)

Lecteur multimédia de bureau multiplateforme (Linux, Windows, macOS) écrit
entièrement en **Rust**, bâti sur **FFmpeg** (décodage), **Slint** (interface,
rendu GPU) et **cpal** (audio). Objectif : un concurrent open source de VLC,
propre, modulaire et performant.

## Fonctionnalités

### Lecture
- **Vidéo** : MP4, MKV, AVI, MOV, WebM, MPEG, FLV, TS, WMV… (tout ce que
  libavformat/libavcodec sait ouvrir)
- **Audio** : MP3, FLAC, WAV, OGG, AAC, M4A, Opus, WMA…
- **Streaming** : HTTP/HTTPS progressif, HLS (`.m3u8`), RTSP (transport TCP),
  IPTV (UDP/RTP multicast), avec reconnexion automatique et timeouts

### Sous-titres
- Formats **SRT, ASS, SSA, WebVTT** (fichiers externes, chargement manuel)
- Pistes **embarquées** (SubRip/ASS dans MKV/MP4), sélection à la volée
- **Décalage réglable** (±0,5 s) pour la synchronisation

### Contrôles
- Lecture / pause / stop, avance & retour rapides (±10 s)
- Vitesse **0.25× à 4×**, volume, sourdine
- **Plein écran** (double-clic ou `F`), **capture d'écran** PNG (`S`)
- Raccourcis : `Espace` lecture/pause, `←`/`→` seek, `↑`/`↓` volume, `M` muet

### Playlist
- Ajout (multi-sélection), suppression, réorganisation (▲/▼)
- Sauvegarde / chargement **M3U** (compatible VLC)
- Enchaînement automatique, piste suivante/précédente

### Confort
- **Reprise automatique** à la dernière position
- **Historique** de lecture (50 entrées)
- Thèmes **clair / sombre**, paramètres **persistants** (JSON)
- Plusieurs pistes **audio** et **sous-titres** par média

## Compilation rapide (Linux)

```bash
# Dépendances (Debian/Ubuntu)
sudo apt install build-essential pkg-config clang \
    libavcodec-dev libavformat-dev libavutil-dev libswscale-dev \
    libswresample-dev libavfilter-dev libavdevice-dev libpostproc-dev \
    libasound2-dev

cargo run --release
```

Instructions détaillées pour **Windows, macOS et Linux** : [docs/BUILD.md](docs/BUILD.md).
Packaging (`.deb`, `.AppImage`, `.exe`, `.dmg`) : [docs/PACKAGING.md](docs/PACKAGING.md).
Architecture interne : [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Utilisation

```bash
oxiplay film.mkv episode2.mp4          # fichiers locaux
oxiplay https://exemple.com/live.m3u8  # flux HLS
oxiplay rtsp://192.168.1.20:554/cam    # caméra RTSP
```

ou lancez l'interface et utilisez 📂 (fichiers), le champ URL (réseau),
💬 (sous-titres externes), ☰ (playlist).

## Qualité

- `cargo test` — 25 tests (unitaires + intégration : le pipeline complet
  décode un vrai fichier, seek, pause, vitesse, fin de lecture)
- `cargo clippy --all-targets` — zéro avertissement
- `cargo fmt --check`, `cargo audit` (zéro CVE), `cargo machete`
- CI GitHub Actions : Linux + Windows + macOS

## État du projet & feuille de route

MVP fonctionnel. Prochaines étapes (voir [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)) :

- [ ] Accélération matérielle (VAAPI, NVDEC, VideoToolbox, D3D11VA)
- [ ] Rendu wgpu zéro copie (textures YUV + shader) pour le 4K/HDR
- [ ] Vitesse sans changement de hauteur (filtre `atempo`)
- [ ] Égaliseur audio 10 bandes, filtres vidéo
- [ ] Sous-titres bitmap (PGS/DVD), rendu stylé libass
- [ ] Mini-lecteur / Picture-in-Picture

## Contribuer

Les contributions sont les bienvenues — voir [CONTRIBUTING.md](CONTRIBUTING.md).
Historique des versions : [CHANGELOG.md](CHANGELOG.md).

## Licence

[GPL-3.0-or-later](LICENSE) — comme l'écosystème FFmpeg qu'il utilise.
