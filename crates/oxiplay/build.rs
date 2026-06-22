fn main() {
    // Le compilateur Slint est récursif ; sur la pile principale de Windows
    // (1 Mo) une interface chargée provoque un STACK_OVERFLOW. On compile donc
    // dans un thread à grande pile (sans effet sur Linux/macOS, pile 8 Mo).
    std::thread::Builder::new()
        .stack_size(32 * 1024 * 1024)
        .spawn(|| {
            slint_build::compile("ui/main.slint")
                .expect("échec de compilation des fichiers .slint");
        })
        .expect("échec du lancement du thread de compilation Slint")
        .join()
        .expect("le thread de compilation Slint a paniqué");
}
