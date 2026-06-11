# Architecture d'OxiPlay

## Vue d'ensemble

OxiPlay sépare strictement **le moteur de lecture** (sans aucune dépendance à
l'interface) de **la couche application/UI**. Le moteur est piloté par des
commandes et publie son état dans une structure partagée ; l'interface n'est
qu'un client parmi d'autres possibles (les tests d'intégration en sont un
deuxième).

## Diagramme des composants

```text
                              ┌────────────────────────────────────────────────┐
                              │                 Thread UI (Slint)              │
                              │  main.slint ⇄ ui/ ⇄ app/ (callbacks + timer)   │
                              └───────┬──────────────────────────▲─────────────┘
                  commandes (canaux,  │                          │  images RGBA (invoke_from_event_loop)
                  atomiques)          │                          │  état (SharedState, 10 Hz)
                              ┌───────▼──────────────────────────┴─────────────┐
                              │              player/ PlayerEngine              │
                              │   SharedState : horloge, génération, pistes,   │
                              │   sous-titres, erreurs, volume, vitesse…       │
                              └───────┬───────────────────────────▲────────────┘
                                      │ DemuxCommand              │
        ┌─────────────────────────────▼───────────┐               │
        │            decoder/demux  (thread)      │               │
        │  libavformat : fichier, HTTP, HLS, RTSP │               │
        │  + décodage sous-titres embarqués       │               │
        └───────┬─────────────────────┬───────────┘               │
   PacketMsg    │ (canal borné 60)    │ (canal borné 120)         │
        ┌───────▼───────────┐ ┌───────▼───────────┐               │
        │ decoder/video     │ │ decoder/audio     │               │
        │ (thread)          │ │ (thread)          │               │
        │ libavcodec        │ │ libavcodec        │               │
        │ + swscale → RGBA  │ │ + swresample      │               │
        └───────┬───────────┘ │ → f32 stéréo      │               │
 VideoFrameMsg  │ (canal 6)   └───────┬───────────┘               │
        ┌───────▼───────────┐         │ échantillons              │
        │ player/presenter  │ ┌───────▼───────────┐               │
        │ (thread)          │ │ audio/AudioQueue  │               │
        │ cadence sur       │ │ (≈1 s, bornée)    │               │
        │ l'horloge,        │ └───────┬───────────┘               │
        │ drop des retards  │         │ callback temps réel       │
        └───────────────────┘ ┌───────▼───────────┐               │
                              │ cpal (ALSA/WASAPI │ ──────────────┘
                              │ /CoreAudio)       │   sync_to(pts) : l'audio
                              └───────────────────┘   pilote l'horloge maîtresse
```

## Modules

| Module        | Rôle | Dépend de |
|---------------|------|-----------|
| `app/`        | Couche application : relie moteur, playlist, paramètres, UI | tous |
| `player/`     | Moteur : threads, horloge (`clock.rs`), état partagé (`state.rs`), présentation | decoder, audio, video |
| `decoder/`    | Demuxage (`demux.rs`), décodage vidéo (`video.rs`) et audio (`audio.rs`) via ffmpeg-next | subtitles, streaming, video |
| `audio/`      | Sortie cpal, file d'échantillons, volume, horloge | player::state |
| `video/`      | Types d'images décodées, capture d'écran PNG | — |
| `subtitles/`  | Parseurs SRT/VTT/ASS/SSA, requêtes par temps | — |
| `playlist/`   | Modèle de liste, M3U | streaming, utils |
| `streaming/`  | Classification d'URL, options libavformat par protocole | — |
| `settings/`   | Persistance JSON : volume, thème, historique, reprise | — |
| `ui/`         | Code généré Slint + conversions | video |
| `utils/`      | Formatage temps, noms affichables | — |

Le graphe de dépendances est **acyclique** et chaque module est testable
isolément (principe de responsabilité unique ; le moteur dépend
d'abstractions — `FrameSink`, `AudioQueue` — pas de Slint ni de cpal
directement : inversion de dépendances).

## Décisions techniques

### Pourquoi Slint ?

| Critère | Slint | egui | Iced | Tauri |
|---|---|---|---|---|
| Rendu GPU natif (femtovg/Skia, OpenGL/Vulkan/Metal) | ✅ | ✅ | ✅ | ❌ (WebView) |
| Mode *retained* (pas de redraw permanent) | ✅ | ❌ (immediate) | ✅ | — |
| Empreinte mémoire | très faible | faible | moyenne | élevée (WebView) |
| Langage UI déclaratif compilé, hot-reload | ✅ | ❌ | ❌ | HTML/JS |
| Accessibilité, i18n | ✅ | partielle | partielle | ✅ |

Pour un lecteur vidéo, le point décisif est le **mode retained** : l'interface
ne se redessine que lorsque l'état change, donc le CPU reste disponible pour
le décodage ; egui (immediate mode) redessine en continu. Tauri imposerait de
faire transiter chaque image par un WebView — rédhibitoire. Iced reste une
alternative crédible mais son écosystème widgets/plateformes est moins mûr.

### Pipeline de threads & synchronisation A/V

- **1 thread de demuxage** (I/O, éventuellement réseau — jamais dans l'UI),
  **1 thread de décodage vidéo**, **1 thread de décodage audio**,
  **1 thread de présentation**, **le callback temps réel cpal**, et **le
  thread UI**. Tous communiquent par canaux bornés (`crossbeam-channel`) :
  la contre-pression régule naturellement la mémoire (≈ 60 paquets vidéo,
  120 paquets audio, 6 images décodées, 1 s d'audio).
- **Horloge maîtresse audio** : le callback cpal avance un PTS au rythme des
  échantillons réellement joués et ré-ancre l'horloge (`PlaybackClock`) si la
  dérive dépasse 30 ms. Sans piste audio, l'horloge court sur le temps mural.
- **Générations de seek** : chaque seek incrémente un compteur atomique ;
  tout paquet/image étiqueté d'une génération antérieure est jeté sans
  traitement. C'est ce qui rend les seeks instantanés pipeline plein, sans
  vidage compliqué des canaux, et qui évite les deadlocks seek-en-pause
  (le présentateur draine les images périmées même en pause).
- **Vitesse** : l'horloge est mise à l'échelle, et l'audio est rééchantillonné
  vers `taux_périphérique / vitesse` (la hauteur change ; un filtre `atempo`
  préservant la hauteur est prévu via libavfilter).

### Pourquoi pas Tokio (pour l'instant)

Tout le pipeline est limité par du **travail CPU bloquant** (décodage) et des
**API bloquantes** (libavformat fait son propre I/O réseau, cpal impose un
callback temps réel). Des threads OS dédiés avec canaux bornés sont l'outil
adapté ; un exécuteur async n'apporterait ici que de la complexité. Tokio
devient pertinent pour les fonctions annexes prévues (téléchargement de
playlists IPTV, métadonnées en ligne, télécommande HTTP) et sera introduit à
ce moment-là, confiné au module `streaming`.

### Rendu vidéo : état actuel et voie wgpu

Aujourd'hui : conversion YUV→RGBA par libswscale dans le thread de décodage,
puis téléversement GPU par Slint (femtovg/Skia au-dessus d'OpenGL/Vulkan/
Metal selon la plateforme). Simple, robuste, et suffisant jusqu'au 1080p60.

Voie prévue pour 4K/HDR (« zéro copie ») : sortir du décodeur les plans YUV
(voire les surfaces GPU de l'accélération matérielle), les téléverser tels
quels en textures wgpu et faire la conversion colorimétrique (BT.709/BT.2020,
tone-mapping HDR) dans un shader, intégré à la scène Slint via son API
`unstable-wgpu` / underlay OpenGL. Le découplage actuel (le moteur livre des
`VideoFrameData` opaques via `FrameSink`) permet ce changement sans toucher
au pipeline.

### Accélération matérielle (feuille de route)

`decoder/video.rs` est le seul point à modifier : demander à libavcodec un
`hw_device_ctx` (VAAPI sous Linux, D3D11VA/DXVA2 sous Windows, VideoToolbox
sous macOS, NVDEC via CUDA), recevoir des trames GPU (`AV_PIX_FMT_VAAPI`…),
et soit les rapatrier (`av_hwframe_transfer_data`, déjà compatible avec le
rendu actuel), soit les passer directement à la voie wgpu. L'API ffmpeg-next
n'expose pas tout ; quelques appels `ffi::` ciblés seront nécessaires.

## Flux d'un seek (exemple de bout en bout)

1. L'utilisateur clique la barre → `seek-to(0.42)` → `App::seek_fraction`.
2. `PlayerEngine::seek` envoie `DemuxCommand::Seek(t)` (try_send, jamais bloquant).
3. Le demuxeur fusionne les seeks en attente, incrémente la **génération**,
   vide l'`AudioQueue`, appelle `avformat_seek_file` (keyframe ≤ cible),
   repositionne l'horloge, envoie `Flush` + `Reconfigure` aux décodeurs.
4. Les décodeurs jettent les paquets périmés (génération), `flush()` leurs
   tampons internes, et repartent sur la nouvelle position.
5. Le présentateur jette les images périmées ; la première image de la
   nouvelle génération est affichée immédiatement si la lecture est en pause
   (aperçu), sinon cadencée normalement.

## Tests

- **Unitaires** (23) : parseurs de sous-titres, playlist/M3U, horloge,
  conversions de temps, paramètres/reprise, classification d'URL, PNG.
- **Intégration** (2, `tests/playback.rs`) : génèrent un vrai MP4 (mire +
  tonalité) avec l'outil `ffmpeg`, puis vérifient sur le moteur complet :
  découverte des pistes, durée, décodage cadencé, pause figée, seek avec
  aperçu, vitesse 2×, fin de lecture, arrêt propre des threads < 3 s.
