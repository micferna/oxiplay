# Changelog

Toutes les évolutions notables de ce projet sont documentées ici.
Format inspiré de [Keep a Changelog](https://keepachangelog.com/fr/1.1.0/) ;
le projet suit [SemVer](https://semver.org/lang/fr/).

## [Non publié]

### Ajouté
- **Vitesse de lecture sans changement de hauteur** : le contrôle 0.25×–4×
  passe par un graphe libavfilter (`atempo`, chaîné pour couvrir toute la
  plage) au lieu du rééchantillonnage — la voix n'est plus transposée.
- **Égaliseur audio 10 bandes** (31 Hz → 16 kHz) : filtres `equalizer`
  libavfilter, panneau d'interface avec sliders verticaux, gains persistés.

### Modifié
- Pipeline audio refondu autour d'un graphe de filtres (égaliseur → atempo →
  conversion), reconstruit à la volée quand la vitesse ou les gains changent.

## [0.1.0] — 2026-06-12

Première version publique (MVP).

### Ajouté
- Lecture vidéo et audio de tous les formats gérés par FFmpeg 7
  (MP4, MKV, AVI, MOV, WebM, MPEG, FLV, TS, WMV / MP3, FLAC, WAV, OGG,
  AAC, M4A, Opus, WMA…).
- Streaming réseau : HTTP/HTTPS, HLS (`.m3u8`), RTSP (transport TCP),
  IPTV UDP/RTP, avec reconnexion automatique.
- Sous-titres SRT, ASS, SSA et WebVTT (fichiers externes) ; pistes
  embarquées (SubRip/ASS) avec sélection à la volée ; décalage réglable.
- Contrôles complets : lecture/pause/stop, avance/retour ±10 s, vitesse
  0.25×–4×, volume, sourdine, plein écran, capture d'écran PNG,
  raccourcis clavier.
- Playlist : ajout, suppression, réorganisation, sauvegarde/chargement M3U,
  enchaînement automatique.
- Reprise automatique à la dernière position, historique de lecture,
  thèmes clair/sombre, paramètres persistants.
- Multi-pistes audio et sous-titres.
- Moteur multi-threads (demux / décodage vidéo / décodage audio /
  présentation) avec horloge maîtresse audio et seeks par génération.
- Tests unitaires (23) et d'intégration (2), CI GitHub Actions
  (Linux, macOS, Windows, audit RustSec).
