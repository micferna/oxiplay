# Packaging

Toutes les commandes se lancent à la racine du dépôt après un
`cargo build --release` réussi.

## Linux — `.deb`

Les métadonnées `[package.metadata.deb]` sont déjà dans
`crates/oxiplay/Cargo.toml` (dépendances vers les bibliothèques runtime
FFmpeg de Debian 13, entrée de menu `.desktop`).

```bash
cargo install cargo-deb
cargo deb -p oxiplay
# → target/debian/oxiplay_0.1.0-1_amd64.deb
sudo apt install ./target/debian/oxiplay_*.deb
```

## Linux — `.AppImage`

L'AppImage embarque les bibliothèques FFmpeg : elle fonctionne sur toute
distribution récente.

```bash
# Outils
wget https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-x86_64.AppImage
chmod +x linuxdeploy-x86_64.AppImage

# Arborescence AppDir
mkdir -p AppDir/usr/bin
cp target/release/oxiplay AppDir/usr/bin/
cp packaging/oxiplay.desktop AppDir/
cp crates/oxiplay/ui/icon.png AppDir/oxiplay.png

./linuxdeploy-x86_64.AppImage --appdir AppDir \
    -e AppDir/usr/bin/oxiplay \
    -d AppDir/oxiplay.desktop -i AppDir/oxiplay.png \
    --output appimage
# → OxiPlay-x86_64.AppImage
```

`linuxdeploy` copie automatiquement les `.so` FFmpeg/ALSA référencés par le
binaire dans l'AppImage.

## Windows — `.exe` (installeur)

Le binaire release est autonome hormis les DLL FFmpeg.

```powershell
cargo build --release
mkdir dist; copy target\release\oxiplay.exe dist\
copy %FFMPEG_DIR%\bin\av*.dll dist\
copy %FFMPEG_DIR%\bin\sw*.dll dist\
```

Installeur au choix :
- **cargo-wix** (MSI) : `cargo install cargo-wix && cargo wix -p oxiplay`
  (ajouter les DLL dans `wix/main.wxs`) ;
- **Inno Setup** : un script qui embarque `dist\*` et crée les associations
  de fichiers.

## macOS — `.dmg`

```bash
cargo install cargo-bundle
cargo bundle --release -p oxiplay        # → OxiPlay.app

# Embarquer les dylib FFmpeg dans l'app (sinon dépendance à Homebrew)
dylibbundler -od -b \
  -x target/release/bundle/osx/OxiPlay.app/Contents/MacOS/oxiplay \
  -d target/release/bundle/osx/OxiPlay.app/Contents/Frameworks/

# Image disque
brew install create-dmg
create-dmg --volname "OxiPlay" --app-drop-link 400 200 \
  OxiPlay.dmg target/release/bundle/osx/OxiPlay.app
```

Pour la distribution hors App Store : signer (`codesign --deep`) puis
notariser (`xcrun notarytool submit`).

## Licences

FFmpeg est sous LGPL/GPL selon les options de compilation : OxiPlay est
distribué en **GPL-3.0-or-later**, compatible dans tous les cas. Les paquets
doivent inclure les mentions FFmpeg, Slint (GPL-3.0/commercial/royalty-free
desktop) et cpal (Apache-2.0).
