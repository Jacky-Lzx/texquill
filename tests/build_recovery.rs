use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use texquill::{compilation::build, config::BuildConfig};

#[test]
#[cfg(unix)]
fn cancelled_and_timed_out_builds_recover_auxiliaries_on_the_next_invocation() {
    for cancelled in [true, false] {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let main = root.join("main.tex");
        std::fs::write(
            &main,
            "\\documentclass{article}\n\\input{out/main.out}\n\\include{chapters/one}\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("chapters")).unwrap();
        std::fs::write(root.join("chapters/one.tex"), "Chapter").unwrap();
        let output = root.join("out");
        std::fs::create_dir_all(output.join("chapters")).unwrap();
        for (name, text) in [
            ("main.pdf", "previous-pdf"),
            ("main.synctex.gz", "previous-synctex"),
            ("main.out", "protected-input"),
            ("other.aux", "another-job"),
            ("refs.bib", "database"),
            ("main.bbl", "supplied-bibliography"),
        ] {
            std::fs::write(output.join(name), text).unwrap();
        }
        let compiler = root.join("compiler.sh");
        std::fs::write(
            &compiler,
            r#"out=$1
if [ ! -f "$out/interrupted" ]; then
  printf '\\gdef\\unfinished{' > "$out/main.aux"
  printf '<incomplete' > "$out/main.bcf"
  printf '\\gdef\\unfinished{' > "$out/chapters/one.aux"
  touch "$out/interrupted"
  while :; do sleep 0.02; done
fi
for name in main.aux main.bcf chapters/one.aux; do
  [ ! -e "$out/$name" ] || exit 12
done
[ "$(cat "$out/main.pdf")" = previous-pdf ] || exit 13
[ "$(cat "$out/main.out")" = protected-input ] || exit 14
[ "$(cat "$out/other.aux")" = another-job ] || exit 15
[ "$(cat "$out/main.synctex.gz")" = previous-synctex ] || exit 16
[ "$(cat "$out/main.bbl")" = supplied-bibliography ] || exit 17
printf '%%PDF-1.4\nrecovered\n' > "$out/main.pdf"
"#,
        )
        .unwrap();
        let config = BuildConfig {
            executable: "/bin/sh".into(),
            arguments: Some(vec![compiler.to_string_lossy().into_owned(), "%o".into()]),
            output_directory: Some("out".into()),
            timeout_seconds: 1,
            ..Default::default()
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let worker_root = main.clone();
        let worker_config = config.clone();
        let worker =
            std::thread::spawn(move || build::run(&worker_root, &worker_config, &worker_cancel));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !output.join("interrupted").exists() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        if cancelled {
            cancel.store(true, Ordering::Relaxed);
        }
        let first = worker.join().unwrap();
        assert_eq!(
            first.status,
            if cancelled { "cancelled" } else { "timeout" }
        );
        assert!(
            std::fs::metadata(output.join("main.texquill-state"))
                .unwrap()
                .len()
                > 0
        );
        let next = build::run(&main, &config, &AtomicBool::new(false));
        assert_eq!(next.status, "success", "{}\n{}", next.message, next.output);
        assert_eq!(
            std::fs::metadata(output.join("main.texquill-state"))
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            std::fs::read_to_string(output.join("refs.bib")).unwrap(),
            "database"
        );
    }
}

#[test]
#[ignore = "requires latexmk and XeLaTeX"]
fn real_xelatex_recovers_a_truncated_auxiliary_after_cancellation() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let main = root.join("main.tex");
    let source = format!("\\documentclass{{article}}\n\\begin{{document}}\n\\makeatletter\n\\immediate\\write\\@auxout{{\\string\\gdef\\string\\probe{{{}}}}}\n\\loop\\ifnum1=1\\repeat\n\\end{{document}}\n", "x".repeat(20000));
    std::fs::write(&main, source).unwrap();
    let config = BuildConfig {
        engine: Some(texquill::config::Engine::XeLatex),
        ..Default::default()
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = cancel.clone();
    let worker_main = main.clone();
    let worker_config = config.clone();
    let worker =
        std::thread::spawn(move || build::run(&worker_main, &worker_config, &worker_cancel));
    let aux = root.join("build/texquill/main.aux");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !std::fs::metadata(&aux).is_ok_and(|meta| meta.len() >= 8192) {
        assert!(
            Instant::now() < deadline,
            "XeLaTeX did not reach the auxiliary write"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    cancel.store(true, Ordering::Relaxed);
    assert_eq!(worker.join().unwrap().status, "cancelled");
    let partial = std::fs::read_to_string(aux).unwrap();
    assert!(partial.matches('{').count() > partial.matches('}').count());
    std::fs::write(
        &main,
        "\\documentclass{article}\n\\begin{document}Recovered.\\end{document}\n",
    )
    .unwrap();
    let next = build::run(&main, &config, &AtomicBool::new(false));
    assert_eq!(next.status, "success", "{}\n{}", next.message, next.output);
    assert_eq!(next.pages, Some(1));
    assert!(!next.output.contains("Runaway definition"));
}
