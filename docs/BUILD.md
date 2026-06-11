# Compilation

OxiPlay nécessite : **Rust stable ≥ 1.80**, **FFmpeg 6, 7 ou 8** avec ses
en-têtes de développement, **clang/libclang** (pour bindgen), et sous Linux
**ALSA** (en-têtes). Le binding `ffmpeg-the-third` détecte la version de
FFmpeg à la compilation : le même code compile contre la 7.x des
distributions et la 8.x récente (la CI teste les deux).

```bash
cargo build --release        # binaire : target/release/oxiplay
cargo test                   # tests unitaires + intégration
```

## Linux (Debian / Ubuntu)

```bash
sudo apt install build-essential pkg-config clang \
    libavcodec-dev libavformat-dev libavutil-dev libswscale-dev \
    libswresample-dev libavfilter-dev libavdevice-dev libpostproc-dev \
    libasound2-dev
cargo build --release
```

Fedora : `sudo dnf install clang pkgconf-pkg-config ffmpeg-devel alsa-lib-devel`
(dépôt RPM Fusion pour FFmpeg complet).
Arch : `sudo pacman -S clang ffmpeg alsa-lib pkgconf`.

### Sans droits root (méthode utilisée par ce dépôt)

Si les paquets `-dev` ne peuvent pas être installés au niveau système mais que
les bibliothèques *runtime* FFmpeg sont présentes (c'est le cas dès que la
commande `ffmpeg` fonctionne), on peut extraire les en-têtes localement :

```bash
mkdir -p .devlibs/debs && cd .devlibs/debs
apt-get download libavcodec-dev libavformat-dev libavutil-dev libswscale-dev \
    libswresample-dev libavfilter-dev libavdevice-dev libpostproc-dev libasound2-dev
for f in *.deb; do dpkg-deb -x "$f" ../root; done
cd ../root/usr/lib/x86_64-linux-gnu
for so in *.so; do ln -sf "/lib/x86_64-linux-gnu/$(readlink "$so")" "$so"; done
cd pkgconfig
sed -i "s|=/usr|=$(pwd)/../../..|" *.pc   # adapte prefix/libdir/includedir
```

Le fichier `.cargo/config.toml` du dépôt ajoute déjà ce répertoire à
`PKG_CONFIG_PATH` (chemin relatif, inoffensif s'il n'existe pas).

## Windows

1. Installer [rustup](https://rustup.rs) (toolchain `x86_64-pc-windows-msvc`)
   et les *Build Tools for Visual Studio* (C++).
2. Installer LLVM (pour libclang) : `winget install LLVM.LLVM`,
   puis `set LIBCLANG_PATH=C:\Program Files\LLVM\bin`.
3. Fournir FFmpeg **shared** 7.x ou 8.x (en-têtes + import libs + DLL) :
   - Builds [BtbN](https://github.com/BtbN/FFmpeg-Builds/releases) (recommandé,
     rapide) : `ffmpeg-n8.1-latest-win64-gpl-shared-8.1.zip`, extraire, puis
     `set FFMPEG_DIR=C:\chemin\ffmpeg-n8.1-...` ;
   - ou gyan.dev (`ffmpeg-release-full-shared.7z`) ;
   - ou vcpkg : `vcpkg install ffmpeg[core,avcodec,avformat,swscale,swresample,avfilter,avdevice]:x64-windows`
     puis `set FFMPEG_DIR=%VCPKG_ROOT%\installed\x64-windows`.
4. `cargo build --release`, puis copier les DLL `av*.dll`, `sw*.dll`
   (depuis `%FFMPEG_DIR%\bin`) à côté de `oxiplay.exe`.

L'audio passe par WASAPI (aucune dépendance supplémentaire).

## macOS

```bash
xcode-select --install        # toolchain C/Cocoa
brew install ffmpeg pkg-config
cargo build --release
```

L'audio passe par CoreAudio, le rendu par Metal (via Slint/Skia). Pour une
app universelle (Apple Silicon + Intel), compiler les deux cibles
(`aarch64-apple-darwin`, `x86_64-apple-darwin`) et fusionner avec `lipo`.

## Variables utiles

| Variable | Effet |
|---|---|
| `FFMPEG_DIR` | Préfixe FFmpeg explicite (prioritaire sur pkg-config) |
| `PKG_CONFIG_PATH` | Répertoires `.pc` supplémentaires |
| `LIBCLANG_PATH` | Emplacement de libclang pour bindgen |
| `RUST_LOG=debug` | Journalisation détaillée à l'exécution |
| `SLINT_BACKEND=winit-skia` | Force un backend de rendu Slint particulier |
