# Changelog

Toutes les évolutions notables de ce projet sont documentées ici.
Format inspiré de [Keep a Changelog](https://keepachangelog.com/fr/1.1.0/) ;
le projet suit [SemVer](https://semver.org/lang/fr/).

## [Non publié]

### Corrigé
- **Couleurs justes en HD/HDR** : swscale est désormais configuré avec l'espace
  colorimétrique (BT.709, BT.2020, BT.601…) et la plage (limitée/complète)
  signalés par le décodeur, au lieu de supposer BT.601 — ce qui décalait les
  couleurs de tout le contenu HD et HDR.
- **Sélecteurs de fichiers non bloquants** : l'ouverture de médias, sous-titres
  et playlists passe par des dialogues **asynchrones** (`slint::spawn_local` +
  `rfd::AsyncFileDialog`). L'ancien dialogue synchrone gelait l'événementiel le
  temps de la navigation, ce que GNOME signalait par « l'application ne répond
  pas / Forcer à quitter ».

### Ajouté
- **Interface bilingue français / anglais** (i18n) : les libellés de l'interface
  sont traduisibles via les traductions *bundlées* de Slint (gettext `.po`
  compilés dans le binaire, sans dépendance système). La langue suit
  automatiquement la locale du système (`LANG`/`LC_ALL`) et peut être forcée
  par le paramètre `language` (`"auto"`, `"fr"`, `"en"`). Le français reste la
  langue source ; l'anglais est fourni (`lang/en/LC_MESSAGES/oxiplay.po`).
- **Sous-titres en ligne** (OpenSubtitles, bouton 🔍) : recherche et
  téléchargement automatique pour le média courant, dans la langue préférée.
  Nécessite une clé d'API gratuite (paramètre `opensubtitles_api_key`).
- **Lecture de vidéos web** (YouTube, Vimeo, Twitch…) via **yt-dlp** : une URL
  de page est automatiquement résolue en flux direct si yt-dlp est installé ;
  repli transparent sur l'URL d'origine sinon (aucune régression).
- **Rendu vidéo GPU (expérimental, feature `gpu`)** : pipeline wgpu YUV→RGB +
  tone-mapping HDR (PQ/HLG) **branché au runtime** et validé à l'écran. Le
  backend wgpu est forcé (avec repli logiciel automatique si l'init GPU échoue),
  son device est capturé via le notifier de rendu Slint, et les images `yuv420p`
  8 bits sont converties par le shader puis importées comme texture
  (`slint::Image::try_from`). Repli RGBA/swscale pour les autres formats, les
  sous-titres image et les captures d'écran. Reste à venir : 10 bits / HDR
  (textures 16 bits) et suppression du double-travail swscale. Voir
  `docs/RENDER_WGPU.md`.
- **Vérification de mise à jour au lancement** : interroge l'API GitHub Releases
  en arrière-plan et signale (bannière cliquable dans la barre d'outils) qu'une
  version plus récente est disponible. Désactivable (`check_updates`).
- **Sélecteur de périphérique de sortie audio** : liste déroulante (affichée
  dès qu'il y a plusieurs sorties) ; la bascule reconstruit la sortie et rouvre
  le média à la même position (le décodeur se reconfigure à la fréquence du
  nouveau matériel).
- **Mémoire par fichier** : la vitesse de lecture, la piste audio et la piste
  de sous-titres choisies sont retrouvées à la réouverture du même média.
- **HUD de statistiques** (touche `H`) : FPS réel, images sautées et décalage
  A/V superposés en haut à droite, pour diagnostiquer la performance de lecture.
- **Contrôles média du bureau** (MPRIS sous Linux ; SMTC/Now Playing à venir
  ailleurs, via `souvlaki`) : touches multimédia du clavier (lecture/pause,
  suivant, précédent, stop, avance/retour) et affichage du titre + état de
  lecture dans l'environnement de bureau.
- **Inhibition de la mise en veille** pendant la lecture vidéo : l'écran ne
  s'éteint plus en plein film (via `systemd-inhibit` sous Linux ; neutre
  ailleurs pour l'instant).
- **Réglages d'image** (bouton 🎨) : luminosité, contraste et saturation via le
  filtre `eq` (sliders + réinitialisation ; effet immédiat, même en pause).
- **Rotation vidéo** (bouton ⟳) : 0 → 90° → 180° → 270°, via le filtre
  `transpose` (composé avec le désentrelacement ; s'applique même en pause).
- **Réglages des sous-titres** (bouton « Aa ») : **taille** ajustable (50–250 %,
  persistée) et **couleur forcée** (blanc/jaune/cyan/vert, ou style ASS
  d'origine).
- **Fiche d'informations média** (bouton ℹ) : conteneur, codecs, résolution,
  cadence, **HDR (PQ/HLG)**, nombre de canaux, débits et durée.
- **Chapitres** : navigation par liste déroulante (titre + horodatage) pour les
  conteneurs qui en fournissent (MKV/MP4, Blu-ray…).
- **Avance image par image** : touches `.` (avant) et `,` (arrière), avec mise
  en pause automatique.
- **Lecture Blu-ray** (libbluray) : ouverture des **dossiers BDMV**, des images
  **`.iso`** et des disques via le protocole `bluray:` — par le bouton 📀, le
  champ URL ou la ligne de commande. Le titre principal le plus long est lu par
  défaut. ⚠️ Les disques **chiffrés** (AACS, et l'AACS 2.0 des **UHD 4K**) ne
  sont pas pris en charge : les clés ne sont pas fournies.
- **Fichiers récents** : liste déroulante dans la barre d'outils (alimentée par
  l'historique de lecture persistant) pour rouvrir un média d'un clic.
- **Désentrelacement automatique** (`yadif`) : les flux entrelacés (DVD, TS
  broadcast, IPTV) sont désentrelacés via un graphe libavfilter, trame par
  trame, **uniquement** quand elles sont marquées entrelacées (le contenu
  progressif passe sans surcoût). Désactivable via `OXIPLAY_NO_DEINTERLACE=1`.
- **Modes de répétition** : boucle désactivée / toute la liste / média courant,
  cyclés par le bouton 🔁 ou la touche `R`.
- **Préréglages d'égaliseur** (Plat, Rock, Pop, Jazz, Classique, Graves+, Voix)
  dans le panneau de l'égaliseur.
- **Décalage de synchronisation audio/vidéo** réglable (boutons `A/V ±`,
  persisté), en complément du décalage des sous-titres.
- **Benchmarks** (`cargo bench`) des chemins chauds Rust : parsing de
  sous-titres SRT/VTT/ASS, recherche de réplique, compositing image.
- **Vitesse de lecture sans changement de hauteur** : le contrôle 0.25×–4×
  passe par un graphe libavfilter (`atempo`, chaîné pour couvrir toute la
  plage) au lieu du rééchantillonnage — la voix n'est plus transposée.
- **Égaliseur audio 10 bandes** (31 Hz → 16 kHz) : filtres `equalizer`
  libavfilter, panneau d'interface avec sliders verticaux, gains persistés.

- **Sous-titres image PGS/DVD** : les bitmaps palettisés du décodeur sont
  convertis en RGBA et incrustés sur l'image vidéo (donc mis à l'échelle
  avec elle).
- **Rendu stylé des sous-titres ASS/SSA** (natif, sans libass) : alignement
  (`\an`/`\a`, 9 positions), gras, italique et couleur primaire sont
  interprétés et appliqués à l'affichage.
- **Décodage vidéo accéléré matériellement** : VAAPI (Linux), NVDEC/CUDA,
  VideoToolbox (macOS), D3D11VA/DXVA2 (Windows), VDPAU. Détection
  automatique du meilleur périphérique, rapatriement des trames GPU vers le
  pipeline RGBA, et **repli logiciel transparent** si aucune accélération
  n'est disponible. Désactivable via `OXIPLAY_NO_HWACCEL=1`.
- **Mini-lecteur** (équivalent bureau du Picture-in-Picture) : fenêtre
  compacte sans habillage avec contrôles flottants au survol (précédent,
  lecture/pause, suivant, retour), basculable par bouton ou la touche `P`.

### Modifié
- **Chemin vidéo** : la trame RGBA de sortie de swscale est réutilisée d'une
  image à l'autre (une allocation par image en moins), et la copie compacte se
  fait en un seul bloc quand le stride le permet.
- **Callback audio temps réel sans blocage** : les verrous y sont désormais pris
  en `try_lock` — en cas de contention (décodeur qui pousse, attache/détache),
  on émet un tampon de silence plutôt que de risquer une inversion de priorité.
- **Compositing des sous-titres image** optimisé : clipping de la zone visible
  en une fois (au lieu d'un test de bornes par pixel) — ~7 % plus rapide dans
  le pire cas, davantage en pratique (sortie identique, vérifiée par les tests).
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
