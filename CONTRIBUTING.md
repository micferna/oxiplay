# Contribuer à OxiPlay

Merci de votre intérêt ! Les contributions sont les bienvenues : rapports de
bugs, documentation, code.

## Mise en place

Voir [docs/BUILD.md](docs/BUILD.md) pour les dépendances par plateforme, puis :

```bash
cargo build
cargo test
```

## Avant d'ouvrir une pull request

La CI exige que tout soit vert localement :

```bash
cargo fmt                              # formatage
cargo clippy --all-targets -- -D warnings
cargo test --workspace                 # unitaires + intégration
cargo doc --no-deps                    # la rustdoc doit compiler sans warning
```

## Lignes directrices

- **Architecture** : le moteur (`player/`, `decoder/`, `audio/`) ne doit
  jamais dépendre de Slint ; l'UI passe par `app/` et les abstractions
  (`FrameSink`, `SharedState`). Lire
  [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) avant toute modification du
  pipeline.
- **Tests** : toute logique nouvelle (parseur, modèle, horloge…) arrive avec
  ses tests unitaires ; les changements du pipeline doivent passer les tests
  d'intégration (`tests/playback.rs`).
- **Unsafe** : à éviter ; chaque bloc `unsafe` existant est justifié par un
  commentaire — faites de même si c'est indispensable.
- **Commits** : messages à l'impératif, concis, en français ou en anglais
  (`decoder: gère les frames matérielles VAAPI`).
- **Dépendances** : sobriété ; toute nouvelle dépendance doit passer
  `cargo audit` et être justifiée dans la PR.

## Signaler un bug

Ouvrez une issue avec : plateforme, version, fichier/URL concerné (ou
`ffprobe` du média), sortie de `RUST_LOG=debug oxiplay …`.

## Licence

En contribuant, vous acceptez que votre code soit publié sous
GPL-3.0-or-later.
