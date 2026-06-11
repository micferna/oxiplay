# Politique de sécurité

## Signaler une vulnérabilité

Merci de **ne pas** ouvrir d'issue publique pour une faille de sécurité.
Utilisez les *GitHub Security Advisories* du dépôt
(onglet **Security → Report a vulnerability**) ou écrivez à
`ocb.ketu@gmail.com`. Réponse visée sous 72 h.

## Modèle de menace

OxiPlay traite des **données non fiables par conception** : fichiers
multimédia, sous-titres et flux réseau peuvent être hostiles. La surface
d'attaque, par ordre de risque :

### 1. Bibliothèques FFmpeg (risque le plus élevé, inhérent)

L'essentiel du décodage (conteneurs, codecs vidéo/audio) est délégué aux
bibliothèques C de **FFmpeg**, historiquement la principale source de RCE des
lecteurs multimédia. OxiPlay n'ajoute aucune protection mémoire à ce code :
**la sûreté face à un média piégé est celle de votre FFmpeg.**

Mitigations en place / recommandées :

- **Tenir FFmpeg à jour.** Les paquets `.deb` dépendent des bibliothèques
  système Debian, qui reçoivent les correctifs de sécurité ; les builds
  Windows/macOS doivent embarquer un FFmpeg récent.
- Garde-fou anti-OOM : les résolutions > 16384×16384 sont rejetées avant
  toute allocation (`decoder/video.rs`).
- **À venir (voir ARCHITECTURE.md)** : exécution du décodage dans un
  processus *sandboxé* (seccomp/Landlock sous Linux, App Sandbox sous macOS)
  — c'est ce que font Chrome/Firefox, et la prochaine étape de durcissement
  la plus impactante.

### 2. Protocoles & playlists réseau (mitigé)

libavformat suit les URL imbriquées d'un manifeste HLS ou d'une playlist.
Sans restriction, un `.m3u8`/`.m3u` distant malveillant pourrait référencer
`file:`, `concat:`, `subfile:`… et exfiltrer des fichiers locaux (SSRF/LFI).

**Mitigation en place** : toute source réseau impose un `protocol_whitelist`
strict excluant `file` et les protocoles d'enchaînement dangereux
(`streaming/mod.rs`, testé). Les fichiers **locaux** ouverts par
l'utilisateur ne sont pas restreints (c'est son propre système de fichiers).

### 3. Analyseurs internes (risque faible)

Les parseurs de sous-titres (SRT/VTT/ASS/SSA), de playlists (M3U) et le
modèle de lecture sont en **Rust sûr** : pas de corruption mémoire possible.
Le pire cas est un *déni de service* (panique d'un thread, allocation
excessive). Durcissements en place :

- Arithmétique des horodatages de sous-titres en *checked arithmetic* :
  un fichier aux valeurs démesurées renvoie « ignoré », ne panique pas
  (testé, `hostile_timestamps_do_not_panic`).
- Décodage tolérant : un paquet/sous-titre illisible est journalisé et
  ignoré, jamais fatal.

### 4. Bloc `unsafe` (audité)

Un unique bloc `unsafe` (`decoder/audio.rs`) réinterprète le tampon de sortie
du resampler FFmpeg en `&[f32]`. Il est **borné** par une vérification de
longueur explicite juste avant, et l'alignement est garanti par FFmpeg.

## Ce qui n'a PAS encore été fait

Transparence : ces vérifications restent à mener et sont les bienvenues en
contribution —

- Fuzzing des parseurs (`cargo-fuzz`) et du pipeline de décodage.
- Exécution sous **Miri** / ASAN des chemins ne touchant pas le FFI.
- Sandboxing du décodage (cf. §1).
- Revue de la gestion mémoire sous flux réseau adverses prolongés.

## Outils d'analyse en intégration continue

- `cargo audit` (RustSec) à chaque push — **CVE des crates Rust uniquement**,
  ne couvre pas le code C de FFmpeg.
- `cargo clippy -D warnings`, `cargo fmt`, tests unitaires + intégration.
