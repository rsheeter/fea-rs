//! tests of the full compiler, including expected successes and failures

use std::path::{Path, PathBuf};

use crate::{
    compile::{error::CompilerError, Compiler, Opts},
    util::ttx::{self as test_utils, Report, TestCase, TestResult},
    GlyphMap, GlyphName,
};

static ROOT_TEST_DIR: &str = "./test-data/compile-tests";
static GOOD_DIR: &str = "good";
static BAD_DIR: &str = "bad";
static GLYPH_ORDER: &str = "glyph_order.txt";
static BAD_OUTPUT_EXTENSION: &str = "ERR";
static FONTTOOLS_TESTS: &str = "./test-data/fonttools-tests";
static IMPORT_RESOLUTION_TEST: &str = "./test-data/include-resolution-tests/dir1/test1.fea";

// tests taken directly from fonttools; these require some special handling.
#[test]
#[ignore = "disabled so we can use CI"]
fn fonttools_tests() -> Result<(), Report> {
    test_utils::assert_has_ttx_executable();
    test_utils::run_all_tests(FONTTOOLS_TESTS, None).into_error()
}

#[test]
fn should_fail() -> Result<(), Report> {
    let mut results = Vec::new();
    for (glyph_map, tests) in iter_test_groups(BAD_DIR) {
        results.extend(tests.into_iter().map(|path| run_bad_test(path, &glyph_map)));
    }
    test_utils::finalize_results(results).into_error()
}

#[test]
fn import_resolution() {
    let glyph_map = test_utils::make_glyph_map();
    let path = PathBuf::from(IMPORT_RESOLUTION_TEST);
    match test_utils::run_test(path, &glyph_map) {
        Ok(_) => (),
        Err(e) => panic!("{:?}", e.reason),
    }
}

#[test]
fn should_pass() -> Result<(), Report> {
    let mut results = Vec::new();
    for (glyph_map, tests) in iter_test_groups(GOOD_DIR) {
        results.extend(
            tests
                .into_iter()
                .map(|path| test_utils::run_test(path, &glyph_map)),
        );
    }
    test_utils::finalize_results(results).into_error()
}

fn iter_test_groups(test_dir: &str) -> impl Iterator<Item = (GlyphMap, Vec<PathBuf>)> + '_ {
    iter_test_group_dirs(ROOT_TEST_DIR).map(move |dir| {
        let glyph_order_path = dir.join(GLYPH_ORDER);
        let glyph_order =
            std::fs::read_to_string(glyph_order_path).expect("failed to read glyph order");
        let glyph_map = glyph_order.lines().map(GlyphName::new).collect();
        let tests_dir = dir.join(test_dir);
        let tests = test_utils::iter_fea_files(tests_dir).collect::<Vec<_>>();
        (glyph_map, tests)
    })
}

fn iter_test_group_dirs(root_dir: impl AsRef<Path>) -> impl Iterator<Item = PathBuf> {
    let mut dir = root_dir.as_ref().read_dir().unwrap();
    std::iter::from_fn(move || loop {
        let entry = dir.next()?.unwrap();
        let path = entry.path();
        if path.is_dir() {
            return Some(path);
        }
    })
}

fn run_bad_test(path: PathBuf, map: &GlyphMap) -> Result<PathBuf, TestCase> {
    match std::panic::catch_unwind(|| bad_test_body(&path, map)) {
        Err(_) => Err(TestCase {
            path,
            reason: TestResult::Panic,
        }),
        Ok(Err(reason)) => Err(TestCase { path, reason }),
        Ok(_) => Ok(path),
    }
}

fn bad_test_body(path: &Path, glyph_map: &GlyphMap) -> Result<(), TestResult> {
    match Compiler::new(path, glyph_map)
        .verbose(std::env::var(crate::util::VERBOSE).is_ok())
        .with_opts(Opts::new().make_post_table(true))
        .compile_binary()
    {
        Ok(_) => Err(TestResult::UnexpectedSuccess),
        // this means we have a test case that doesn't exist or something weird
        Err(CompilerError::SourceLoad(err)) => panic!("{err}"),
        Err(CompilerError::WriteFail(err)) => panic!("{err}"),
        Err(CompilerError::ParseFail(errs)) => Err(TestResult::ParseFail(errs.to_string())),
        Err(CompilerError::ValidationFail(errs) | CompilerError::CompilationFail(errs)) => {
            let msg = errs.to_string();
            let result = test_utils::compare_to_expected_output(&msg, path, BAD_OUTPUT_EXTENSION);
            if result.is_err() && std::env::var(crate::util::WRITE_RESULTS_VAR).is_ok() {
                let to_path = path.with_extension(BAD_OUTPUT_EXTENSION);
                std::fs::write(to_path, &msg).expect("failed to write output");
            }
            result.map_err(|e| e.reason)
        }
    }
}
