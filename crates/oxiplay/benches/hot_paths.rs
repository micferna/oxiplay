//! Benchmarks des chemins chauds **en Rust pur** (sans décodage FFmpeg).
//!
//! Mesure ce qui s'exécute par image (compositing des sous-titres image),
//! par tick d'interface (recherche de sous-titre) ou à l'ouverture d'un
//! média (parsing des fichiers de sous-titres). Le décodage vidéo/audio
//! lui-même est dominé par libavcodec et n'est pas micro-benchmarkable ici ;
//! il relève d'un bench d'intégration (latence de seek) séparé.
//!
//! Lancer : `cargo bench -p oxiplay`

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use oxiplay::streaming::classify;
use oxiplay::subtitles::bitmap::composite;
use oxiplay::subtitles::{
    parse_ass, parse_srt, parse_vtt, BitmapSubtitle, SubtitleCue, SubtitleTrack,
};
use oxiplay::utils::format_time;

/// Horodatage SRT/VTT à partir de millisecondes.
fn hms_ms(ms: i64, sep: char) -> String {
    let (h, m, s, milli) = (
        ms / 3_600_000,
        (ms / 60_000) % 60,
        (ms / 1000) % 60,
        ms % 1000,
    );
    format!("{h:02}:{m:02}:{s:02}{sep}{milli:03}")
}

fn make_srt(n: usize) -> String {
    let mut out = String::with_capacity(n * 48);
    for i in 0..n {
        let start = i as i64 * 2000;
        out.push_str(&format!(
            "{}\n{} --> {}\nRéplique numéro {i} sur deux lignes\nseconde ligne\n\n",
            i + 1,
            hms_ms(start, ','),
            hms_ms(start + 1500, ','),
        ));
    }
    out
}

fn make_vtt(n: usize) -> String {
    let mut out = String::from("WEBVTT\n\n");
    for i in 0..n {
        let start = i as i64 * 2000;
        out.push_str(&format!(
            "{} --> {}\nRéplique numéro {i}\n\n",
            hms_ms(start, '.'),
            hms_ms(start + 1500, '.'),
        ));
    }
    out
}

/// Horodatage ASS (H:MM:SS.cc).
fn ass_ts(ms: i64) -> String {
    let (h, m, s, cs) = (
        ms / 3_600_000,
        (ms / 60_000) % 60,
        (ms / 1000) % 60,
        (ms % 1000) / 10,
    );
    format!("{h}:{m:02}:{s:02}.{cs:02}")
}

fn make_ass(n: usize) -> String {
    let mut out = String::from(
        "[Script Info]\nScriptType: v4.00+\n\n\
         [V4+ Styles]\n\
         Format: Name, Fontname, Fontsize, PrimaryColour, Bold, Italic, Alignment\n\
         Style: Default,Arial,20,&H00FFFFFF,0,0,2\n\n\
         [Events]\n\
         Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n",
    );
    for i in 0..n {
        let start = i as i64 * 2000;
        out.push_str(&format!(
            "Dialogue: 0,{},{},Default,,0,0,0,,{{\\i1}}Réplique{{\\i0}} \\N numéro {i}\n",
            ass_ts(start),
            ass_ts(start + 1500),
        ));
    }
    out
}

/// Piste triée de `n` répliques pour les benches de recherche.
fn make_track(n: usize) -> SubtitleTrack {
    let cues = (0..n)
        .map(|i| {
            let start = i as i64 * 2000;
            SubtitleCue::plain(start, start + 1500, format!("Réplique {i}"))
        })
        .collect();
    SubtitleTrack::new(cues)
}

fn bench_subtitle_parsing(c: &mut Criterion) {
    let srt = make_srt(2000);
    let vtt = make_vtt(2000);
    let ass = make_ass(2000);

    let mut g = c.benchmark_group("subtitle_parse_2000_cues");
    g.bench_function("srt", |b| {
        b.iter(|| black_box(parse_srt(black_box(&srt)).ok()))
    });
    g.bench_function("vtt", |b| {
        b.iter(|| black_box(parse_vtt(black_box(&vtt)).ok()))
    });
    g.bench_function("ass", |b| {
        b.iter(|| black_box(parse_ass(black_box(&ass)).ok()))
    });
    g.finish();
}

fn bench_subtitle_query(c: &mut Criterion) {
    // Appelé ~10×/s pendant toute la lecture : doit rester O(log n).
    let track = make_track(5000);
    let mut g = c.benchmark_group("subtitle_query_5000_cues");
    g.bench_function("hit_middle", |b| {
        b.iter(|| black_box(track.query(black_box(5_000_000))))
    });
    g.bench_function("miss_end", |b| {
        b.iter(|| black_box(track.query(black_box(999_999_999))))
    });
    g.finish();
}

fn bench_bitmap_composite(c: &mut Criterion) {
    // Incrustation PGS/DVD : exécutée par image quand des sous-titres image
    // sont actifs. Sous-titre 1280×120 alpha-composité sur une image 1080p.
    let (fw, fh) = (1920u32, 1080u32);
    let (sw, sh) = (1280u32, 120u32);
    let mut frame = vec![16u8; (fw * fh * 4) as usize];
    let mut rgba = vec![0u8; (sw * sh * 4) as usize];
    for px in rgba.chunks_mut(4) {
        px.copy_from_slice(&[255, 220, 40, 180]); // jaune semi-transparent
    }
    let sub = BitmapSubtitle {
        start_us: 0,
        end_us: 1_000_000,
        x: (fw - sw) / 2,
        y: fh - sh - 40,
        width: sw,
        height: sh,
        rgba,
    };
    c.bench_function("bitmap_composite_1280x120_on_1080p", |b| {
        b.iter(|| composite(black_box(&mut frame), fw, fh, black_box(&sub)))
    });
}

fn bench_misc(c: &mut Criterion) {
    let mut g = c.benchmark_group("misc");
    g.bench_function("format_time", |b| {
        b.iter(|| black_box(format_time(black_box(5_025_123_456))))
    });
    g.bench_function("classify_url", |b| {
        b.iter(|| black_box(classify(black_box("https://ex.com/live/master.m3u8?tok=1"))))
    });
    g.finish();
}

criterion_group!(
    benches,
    bench_subtitle_parsing,
    bench_subtitle_query,
    bench_bitmap_composite,
    bench_misc,
);
criterion_main!(benches);
