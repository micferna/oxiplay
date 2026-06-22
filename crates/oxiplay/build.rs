fn main() {
    // Le compilateur Slint est récursif ; sur la pile principale de Windows
    // (1 Mo) une interface chargée provoque un STACK_OVERFLOW. On compile donc
    // dans un thread à grande pile (sans effet sur Linux/macOS, pile 8 Mo).
    std::thread::Builder::new()
        .stack_size(32 * 1024 * 1024)
        .spawn(|| {
            // Traductions bundlées (gettext .po dans lang/<lang>/LC_MESSAGES/),
            // sans contexte par défaut → msgid = la chaîne source telle quelle.
            let config = slint_build::CompilerConfiguration::new()
                .with_bundled_translations("lang")
                .with_default_translation_context(slint_build::DefaultTranslationContext::None);
            slint_build::compile_with_config("ui/main.slint", config)
                .expect("échec de compilation des fichiers .slint");
        })
        .expect("échec du lancement du thread de compilation Slint")
        .join()
        .expect("le thread de compilation Slint a paniqué");
}
