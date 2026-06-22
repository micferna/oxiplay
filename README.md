# OxiPlay 🦀🎬

[![CI](https://github.com/micferna/oxiplay/actions/workflows/ci.yml/badge.svg)](https://github.com/micferna/oxiplay/actions/workflows/ci.yml)
[![Licence: GPL-3.0](https://img.shields.io/badge/licence-GPL--3.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-stable%20%E2%89%A5%201.80-orange.svg)](https://www.rust-lang.org)

Lecteur multimédia de bureau multiplateforme (Linux, Windows, macOS) écrit
entièrement en **Rust**, bâti sur **FFmpeg 7/8** (décodage, via
`ffmpeg-the-third`), **Slint** (interface, rendu GPU) et **cpal** (audio).
Objectif : un concurrent open source de VLC, propre, modulaire et performant.

## Fonctionnalités

### Lecture
- **Vidéo** : MP4, MKV, AVI, MOV, WebM, MPEG, FLV, TS, WMV… (tout ce que
  libavformat/libavcodec sait ouvrir), jusqu'en **4K/8K** (HEVC, AV1, VP9…)
  avec **décodage matériel** (VAAPI/NVDEC/QSV/VideoToolbox/D3D11VA) et
  colorimétrie correcte BT.709/BT.2020
- **Audio** : MP3, FLAC, WAV, OGG, AAC, M4A, Opus, WMA…
- **Blu-ray** : dossiers **BDMV**, images **`.iso`** et disques (libbluray,
  bouton 📀) — disques non chiffrés uniquement (l'AACS des disques commerciaux,
  dont l'UHD 4K, n'est pas pris en charge)
- **Streaming** : HTTP/HTTPS progressif, HLS (`.m3u8`), RTSP (transport TCP),
  IPTV (UDP/RTP multicast), avec reconnexion automatique et timeouts

- **Désentrelacement** automatique (yadif) des flux entrelacés (DVD, TS, IPTV)
- **Chapitres** (MKV/MP4/Blu-ray) : navigation par liste déroulante

### Sous-titres
- Formats **SRT, ASS, SSA, WebVTT** (fichiers externes, chargement manuel)
- Pistes **embarquées** (SubRip/ASS dans MKV/MP4), sélection à la volée
- Sous-titres **image PGS/DVD** incrustés sur la vidéo
- **Décalage réglable** et **style utilisateur** (taille, couleur)

### Contrôles
- Lecture / pause / stop, avance & retour rapides (±10 s), **avance image par image**
- Vitesse **0.25× à 4×** (hauteur préservée), volume, sourdine
- **Rotation** (0/90/180/270°), **plein écran**, **capture d'écran** PNG
- **Modes de répétition** (off / liste / média), **mini-lecteur**
- **HUD de statistiques** (FPS, images sautées, A/V)
- Raccourcis : `Espace`, `←`/`→`, `↑`/`↓`, `M` muet, `F` plein écran,
  `P` mini-lecteur, `S` capture, `R` répétition, `H` stats, `.`/`,` image suivante/précédente

### Image & audio
- **Réglages d'image** : luminosité, contraste, saturation
- **Égaliseur 10 bandes** + préréglages (Rock, Pop, Jazz, Voix…)
- **Décalage de synchronisation A/V** réglable
- **Sélection du périphérique** de sortie audio

### Playlist
- Ajout (multi-sélection), suppression, réorganisation (▲/▼)
- Sauvegarde / chargement **M3U** (compatible VLC)
- Enchaînement automatique, **fichiers récents**

### Intégration bureau
- **Contrôles média** MPRIS/SMTC/Now Playing : touches multimédia du clavier
  et affichage du média en cours dans le bureau
- **Inhibition de la veille** pendant la lecture (l'écran ne s'éteint plus)
- **Vérification de mise à jour** au lancement (release GitHub)

### Confort
- **Reprise automatique** à la dernière position
- **Mémoire par fichier** (vitesse, piste audio, sous-titres) et historique
- **Fiche d'informations média** (codec, résolution, HDR, débits)
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

MVP fonctionnel. Fonctionnalités avancées (voir [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)) :

- [x] **Vitesse sans changement de hauteur** (filtre `atempo`)
- [x] **Égaliseur audio 10 bandes** (libavfilter, UI + persistance)
- [x] **Sous-titres image PGS/DVD** (incrustation RGBA sur la vidéo)
- [x] **Rendu stylé ASS** (alignement, gras, italique, couleur — natif)
- [x] **Accélération matérielle** (VAAPI, NVDEC/CUDA, VideoToolbox, D3D11VA,
      DXVA2, VDPAU) avec repli logiciel automatique
- [x] **Mini-lecteur** (fenêtre compacte, contrôles flottants — touche `P`)
- [ ] Rendu wgpu zéro copie (textures YUV + shader) pour le 4K/HDR
- [ ] libass complet (polices embarquées, karaoké, transformations)
- [ ] Always-on-top du mini-lecteur (dépend du backend de fenêtrage)

## Contribuer

Les contributions sont les bienvenues — voir [CONTRIBUTING.md](CONTRIBUTING.md).
Historique des versions : [CHANGELOG.md](CHANGELOG.md).

## Licence

[GPL-3.0-or-later](LICENSE) — comme l'écosystème FFmpeg qu'il utilise.
