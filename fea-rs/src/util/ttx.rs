//! utilities for compiling and comparing ttx

use std::{
    collections::HashMap,
    env::temp_dir,
    ffi::OsStr,
    fmt::{Debug, Display, Write},
    path::{Path, PathBuf},
    process::Command,
    time::SystemTime,
};

use crate::{
    compile::{
        error::{CompilerError, DiagnosticSet},
        Compiler, Opts,
    },
    Diagnostic, GlyphIdent, GlyphMap, GlyphName, ParseTree,
};

use ansi_term::Color;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

static IGNORED_TESTS: &[&str] = &[
    // ## tests with invalid syntax ## //
    "AlternateChained.fea",
    "GSUB_6.fea",
    //
    // ## tests that should be revisited ## //
    //
    // includes syntax that is (i think) useless, and should at least be a warning
    "GSUB_8.fea",
    // # tests of variable syntax extension #
    "variable_bug2772.fea",
    "variable_conditionset.fea",
    "variable_scalar_anchor.fea",
    "variable_scalar_valuerecord.fea",
];

/// An environment variable that can be set to specify where to write generated files.
///
/// This can be set during debugging if you want to inspect the generated files.
static TEMP_DIR_ENV: &str = "TTX_TEMP_DIR";

/// The combined results of this set of tests
#[derive(Default, Serialize, Deserialize)]
pub struct Report {
    /// All of the test cases for this report
    pub results: Vec<TestCase>,
}

#[derive(Default)]
struct ReportSummary {
    passed: u32,
    panic: u32,
    parse: u32,
    compile: u32,
    compare: u32,
    other: u32,
    sum_compare_perc: f64,
}

struct ReportComparePrinter<'a> {
    old: &'a Report,
    new: &'a Report,
}

/// A specific test and its result
#[derive(Serialize, Deserialize)]
pub struct TestCase {
    /// The path of the input file
    pub path: PathBuf,
    /// The result of running the test
    pub reason: TestResult,
}

/// The result of a ttx test
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub enum TestResult {
    /// The output exactly matched the expectation
    Success,
    /// A panic occured somewhere during compilation
    Panic,
    /// The input could not be parsed
    ParseFail(String),
    /// The input could not be compiled
    CompileFail(String),
    /// Compilation succeeded, but shouldn't have
    UnexpectedSuccess,
    /// A call to the `ttx` utility failed
    #[allow(missing_docs)]
    TtxFail { code: Option<i32>, std_err: String },
    /// The output did not match the expectation
    #[allow(missing_docs)]
    CompareFail {
        expected: String,
        result: String,
        diff_percent: f64,
    },
}

struct ReasonPrinter<'a> {
    verbose: bool,
    reason: &'a TestResult,
}

/// Assert that we can find the `ttx` executable
pub fn assert_has_ttx_executable() {
    assert!(
        Command::new("ttx")
            .arg("--version")
            .status()
            .map(|s| s.success())
            .unwrap_or(false),
        "\nmissing `ttx` executable. Install it with `pip install fonttools`."
    )
}

/// Selectively filter which files to run.
pub struct Filter<'a>(Vec<&'a str>);

impl<'a> Filter<'a> {
    /// Create a new filter from a comma-separated list of inputs
    pub fn new(input: Option<&'a String>) -> Self {
        Self(
            input
                .map(|s| s.split(',').map(|s| s.trim()).collect::<Vec<_>>())
                .unwrap_or_default(),
        )
    }

    /// true if this matches the filter, false if not
    pub fn filter(&self, item: &str) -> bool {
        self.0.is_empty() || self.0.iter().any(|needle| item.contains(needle))
    }
}

/// Run the fonttools tests.
///
/// This compiles the test files, generates ttx, and compares that with what
/// is generated by fonttools.
///
/// `filter` is an optional comma-separated list of strings. If present, only
/// tests which contain one of the strings in the list will be run.
pub fn run_all_tests(fonttools_data_dir: impl AsRef<Path>, filter: Option<&String>) -> Report {
    let glyph_map = make_glyph_map();
    let filter = Filter::new(filter);

    let result = iter_compile_tests(fonttools_data_dir.as_ref(), filter)
        .par_bridge()
        .map(|path| run_test(path, &glyph_map))
        .collect::<Vec<_>>();

    finalize_results(result)
}

/// Convert a vector of test results into a report.
pub fn finalize_results(result: Vec<Result<PathBuf, TestCase>>) -> Report {
    let mut result = result
        .into_iter()
        .fold(Report::default(), |mut results, current| {
            match current {
                Err(e) => results.results.push(e),
                Ok(path) => results.results.push(TestCase {
                    path,
                    reason: TestResult::Success,
                }),
            }
            results
        });
    result.results.sort_unstable_by(|a, b| {
        (a.reason.sort_order(), &a.path).cmp(&(b.reason.sort_order(), &b.path))
    });
    result
}

fn iter_compile_tests<'a>(
    path: &'a Path,
    filter: Filter<'a>,
) -> impl Iterator<Item = PathBuf> + 'a {
    iter_fea_files(path).filter(move |p| {
        if p.extension() == Some(OsStr::new("fea")) && p.with_extension("ttx").exists() {
            let path_str = p.file_name().unwrap().to_str().unwrap();
            if IGNORED_TESTS.contains(&path_str) {
                return false;
            }
            return filter.filter(path_str);
        }
        false
    })
}

/// Iterate over all the files in a directory with the 'fea' suffix
pub fn iter_fea_files(path: impl AsRef<Path>) -> impl Iterator<Item = PathBuf> + 'static {
    let mut dir = path.as_ref().read_dir().unwrap();
    std::iter::from_fn(move || loop {
        let entry = dir.next()?.unwrap();
        let path = entry.path();
        if path.extension() == Some(OsStr::new("fea")) {
            return Some(path);
        }
    })
}

/// Attempt to parse a feature file
pub fn try_parse_file(
    path: &Path,
    glyphs: Option<&GlyphMap>,
) -> Result<ParseTree, (ParseTree, Vec<Diagnostic>)> {
    let (tree, errs) = crate::parse::parse_root_file(path, glyphs, None).unwrap();
    if errs.iter().any(Diagnostic::is_error) {
        Err((tree, errs))
    } else {
        print_diagnostics_if_verbose(&tree, &errs);
        Ok(tree)
    }
}

/// Run the test case at the provided path.
pub fn run_test(path: PathBuf, glyph_map: &GlyphMap) -> Result<PathBuf, TestCase> {
    match std::panic::catch_unwind(|| {
        match Compiler::new(&path, glyph_map)
            .verbose(std::env::var(super::VERBOSE).is_ok())
            .with_opts(Opts::new().make_post_table(true))
            .compile_binary()
        {
            // this means we have a test case that doesn't exist or something weird
            Err(CompilerError::SourceLoad(err)) => panic!("{err}"),
            Err(CompilerError::WriteFail(err)) => panic!("{err}"),
            Err(CompilerError::ParseFail(errs)) => Err(TestResult::ParseFail(errs.to_string())),
            Err(CompilerError::ValidationFail(errs) | CompilerError::CompilationFail(errs)) => {
                Err(TestResult::CompileFail(errs.to_string()))
            }
            Ok(result) => compare_ttx(&result, &path),
        }
    }) {
        Err(_) => Err(TestResult::Panic),
        Ok(Err(reason)) => Err(reason),
        Ok(Ok(_)) => return Ok(path),
    }
    .map_err(|reason| TestCase { reason, path })
}

/// Convert diagnostics to a printable string
pub fn stringify_diagnostics(root: &ParseTree, diagnostics: &[Diagnostic]) -> String {
    DiagnosticSet {
        sources: root.sources.clone(),
        messages: diagnostics.to_owned(),
    }
    .to_string()
}

fn print_diagnostics_if_verbose(root: &ParseTree, diagnostics: &[Diagnostic]) {
    if std::env::var(super::VERBOSE).is_ok() && !diagnostics.is_empty() {
        eprintln!("{}", stringify_diagnostics(root, diagnostics));
    }
}

fn get_temp_dir() -> PathBuf {
    match std::env::var(TEMP_DIR_ENV) {
        Ok(dir) => {
            let dir = PathBuf::from(dir);
            if !dir.exists() {
                std::fs::create_dir_all(&dir).unwrap();
            }
            dir
        }
        Err(_) => temp_dir(),
    }
}

fn get_temp_file_name(in_file: &Path) -> PathBuf {
    let stem = in_file.file_stem().unwrap().to_str().unwrap();
    let millis = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    Path::new(&format!("{stem}_{millis}")).with_extension("ttf")
}

fn compare_ttx(font_data: &[u8], fea_path: &Path) -> Result<(), TestResult> {
    let ttx_path = fea_path.with_extension("ttx");
    let expected_diff_path = fea_path.with_extension("expected_diff");
    let temp_path = get_temp_dir().join(get_temp_file_name(fea_path));
    std::fs::write(&temp_path, font_data).unwrap();

    const TO_WRITE: &[&str] = &[
        "head", "name", "BASE", "GDEF", "GSUB", "GPOS", "OS/2", "STAT", "hhea", "vhea",
    ];

    let mut cmd = Command::new("ttx");
    for table in TO_WRITE {
        cmd.arg("-t").arg(table);
    }
    let status = cmd
        .arg(&temp_path)
        .output()
        .unwrap_or_else(|_| panic!("failed to execute for path {}", fea_path.display()));
    if !status.status.success() {
        let std_err = String::from_utf8_lossy(&status.stderr).into_owned();
        return Err(TestResult::TtxFail {
            code: status.status.code(),
            std_err,
        });
    }

    let ttx_out_path = temp_path.with_extension("ttx");
    assert!(ttx_out_path.exists());

    let result = std::fs::read_to_string(ttx_out_path).unwrap();

    let result = rewrite_ttx(&result);

    let expected = ttx_path
        .exists()
        .then(|| std::fs::read_to_string(&ttx_path).unwrap())
        .unwrap_or_default();
    let expected = rewrite_ttx(&expected);

    if expected_diff_path.exists() {
        let expected_diff = std::fs::read_to_string(&expected_diff_path).unwrap();
        let simple_diff = plain_text_diff(&expected, &result);
        if expected_diff == simple_diff {
            return Ok(());
        }
    }

    if std::env::var(super::WRITE_RESULTS_VAR).is_ok() {
        std::fs::write(&ttx_path, &result).unwrap();
    }
    let diff_percent = compute_diff_percentage(&expected, &result);

    if expected != result {
        Err(TestResult::CompareFail {
            expected,
            result,
            diff_percent,
        })
    } else {
        Ok(())
    }
}

/// take some output and compare it to the expected output (saved on disk)
pub fn compare_to_expected_output(
    output: &str,
    src_path: &Path,
    cmp_ext: &str,
) -> Result<(), TestCase> {
    let cmp_path = src_path.with_extension(cmp_ext);
    let expected = if cmp_path.exists() {
        std::fs::read_to_string(&cmp_path).expect("failed to read cmp_path")
    } else {
        String::new()
    };

    if expected != output {
        let diff_percent = compute_diff_percentage(&expected, output);
        return Err(TestCase {
            path: src_path.to_owned(),
            reason: TestResult::CompareFail {
                expected,
                result: output.to_string(),
                diff_percent,
            },
        });
    }
    Ok(())
}
// hacky way to make our ttx output match fonttools'
fn rewrite_ttx(input: &str) -> String {
    let mut out = String::with_capacity(input.len());

    for line in input.lines() {
        if line.starts_with("<ttFont") {
            out.push_str("<ttFont>\n");
        } else {
            out.push_str(line);
            out.push('\n')
        }
    }
    out
}

fn write_lines(f: &mut impl Write, lines: &[&str], line_num: usize, prefix: char) {
    writeln!(f, "L{}", line_num).unwrap();
    for line in lines {
        writeln!(f, "{}  {}", prefix, line).unwrap();
    }
}

static DIFF_PREAMBLE: &str = "\
# generated automatically by fea-rs
# this file represents an acceptable difference between the output of
# fonttools and the output of fea-rs for a given input.
";

fn compute_diff_percentage(left: &str, right: &str) -> f64 {
    let lines = diff::lines(left, right);
    let same = lines
        .iter()
        .filter(|l| matches!(l, diff::Result::Both { .. }))
        .count();
    let total = lines.len() as f64;
    let perc = (same as f64) / total;

    const PRECISION_SMUDGE: f64 = 10000.0;
    (perc * PRECISION_SMUDGE).trunc() / PRECISION_SMUDGE
}

/// a simple diff (without highlighting) suitable for writing to disk
pub fn plain_text_diff(left: &str, right: &str) -> String {
    let lines = diff::lines(left, right);
    let mut result = DIFF_PREAMBLE.to_string();
    let mut temp: Vec<&str> = Vec::new();
    let mut left_or_right = None;
    let mut section_start = 0;

    for (i, line) in lines.iter().enumerate() {
        match line {
            diff::Result::Left(line) => {
                if left_or_right == Some('R') {
                    write_lines(&mut result, &temp, section_start, '<');
                    temp.clear();
                } else if left_or_right != Some('L') {
                    section_start = i;
                }
                temp.push(line);
                left_or_right = Some('L');
            }
            diff::Result::Right(line) => {
                if left_or_right == Some('L') {
                    write_lines(&mut result, &temp, section_start, '>');
                    temp.clear();
                } else if left_or_right != Some('R') {
                    section_start = i;
                }
                temp.push(line);
                left_or_right = Some('R');
            }
            diff::Result::Both { .. } => {
                match left_or_right.take() {
                    Some('R') => write_lines(&mut result, &temp, section_start, '<'),
                    Some('L') => write_lines(&mut result, &temp, section_start, '>'),
                    _ => (),
                }
                temp.clear();
            }
        }
    }
    match left_or_right.take() {
        Some('R') => write_lines(&mut result, &temp, section_start, '<'),
        Some('L') => write_lines(&mut result, &temp, section_start, '>'),
        _ => (),
    }
    result
}

/// Generate the sample glyph map.
///
/// This is the glyph map used in the feaLib test suite.
pub fn make_glyph_map() -> GlyphMap {
    #[rustfmt::skip]
static TEST_FONT_GLYPHS: &[&str] = &[
    ".notdef", "space", "slash", "fraction", "semicolon", "period", "comma",
    "ampersand", "quotedblleft", "quotedblright", "quoteleft", "quoteright",
    "zero", "one", "two", "three", "four", "five", "six", "seven", "eight",
    "nine", "zero.oldstyle", "one.oldstyle", "two.oldstyle",
    "three.oldstyle", "four.oldstyle", "five.oldstyle", "six.oldstyle",
    "seven.oldstyle", "eight.oldstyle", "nine.oldstyle", "onequarter",
    "onehalf", "threequarters", "onesuperior", "twosuperior",
    "threesuperior", "ordfeminine", "ordmasculine", "A", "B", "C", "D", "E",
    "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P", "Q", "R", "S",
    "T", "U", "V", "W", "X", "Y", "Z", "a", "b", "c", "d", "e", "f", "g",
    "h", "i", "j", "k", "l", "m", "n", "o", "p", "q", "r", "s", "t", "u",
    "v", "w", "x", "y", "z", "A.sc", "B.sc", "C.sc", "D.sc", "E.sc", "F.sc",
    "G.sc", "H.sc", "I.sc", "J.sc", "K.sc", "L.sc", "M.sc", "N.sc", "O.sc",
    "P.sc", "Q.sc", "R.sc", "S.sc", "T.sc", "U.sc", "V.sc", "W.sc", "X.sc",
    "Y.sc", "Z.sc", "A.alt1", "A.alt2", "A.alt3", "B.alt1", "B.alt2",
    "B.alt3", "C.alt1", "C.alt2", "C.alt3", "a.alt1", "a.alt2", "a.alt3",
    "a.end", "b.alt", "c.mid", "d.alt", "d.mid", "e.begin", "e.mid",
    "e.end", "m.begin", "n.end", "s.end", "z.end", "Eng", "Eng.alt1",
    "Eng.alt2", "Eng.alt3", "A.swash", "B.swash", "C.swash", "D.swash",
    "E.swash", "F.swash", "G.swash", "H.swash", "I.swash", "J.swash",
    "K.swash", "L.swash", "M.swash", "N.swash", "O.swash", "P.swash",
    "Q.swash", "R.swash", "S.swash", "T.swash", "U.swash", "V.swash",
    "W.swash", "X.swash", "Y.swash", "Z.swash", "f_l", "c_h", "c_k", "c_s",
    "c_t", "f_f", "f_f_i", "f_f_l", "f_i", "o_f_f_i", "s_t", "f_i.begin",
    "a_n_d", "T_h", "T_h.swash", "germandbls", "ydieresis", "yacute",
    "breve", "grave", "acute", "dieresis", "macron", "circumflex",
    "cedilla", "umlaut", "ogonek", "caron", "damma", "hamza", "sukun",
    "kasratan", "lam_meem_jeem", "noon.final", "noon.initial", "by",
    "feature", "lookup", "sub", "table", "uni0327", "uni0328", "e.fina",
];
    TEST_FONT_GLYPHS
        .iter()
        .map(|name| GlyphIdent::Name(GlyphName::new(*name)))
        .chain((800_u16..=1001).map(GlyphIdent::Cid))
        .collect()
}

impl Report {
    ///  Returns `true` if any tests have failed.
    pub fn has_failures(&self) -> bool {
        self.results.iter().any(|r| !r.reason.is_success())
    }

    /// Convert this type into a Result.
    ///
    /// This result type can be returned from a test method.
    pub fn into_error(self) -> Result<(), Self> {
        if self.has_failures() {
            Err(self)
        } else {
            Ok(())
        }
    }

    /// Return a type that can print comparison results
    pub fn compare_printer<'a, 'b: 'a>(&'b self, old: &'a Report) -> impl std::fmt::Debug + 'a {
        ReportComparePrinter { old, new: self }
    }

    /// returns the number of chars in the widest path
    fn widest_path(&self) -> usize {
        self.results
            .iter()
            .map(|item| &item.path)
            .map(|p| p.file_name().unwrap().to_str().unwrap().chars().count())
            .max()
            .unwrap_or(0)
    }

    fn summary(&self) -> ReportSummary {
        let mut summary = ReportSummary::default();
        for item in &self.results {
            match &item.reason {
                TestResult::Success => summary.passed += 1,
                TestResult::Panic => summary.panic += 1,
                TestResult::ParseFail(_) => summary.parse += 1,
                TestResult::CompileFail(_) => summary.compile += 1,
                TestResult::UnexpectedSuccess | TestResult::TtxFail { .. } => summary.other += 1,
                TestResult::CompareFail { diff_percent, .. } => {
                    summary.compare += 1;
                    summary.sum_compare_perc += diff_percent;
                }
            }
        }
        summary
    }
}

impl TestResult {
    fn sort_order(&self) -> u8 {
        match self {
            Self::Success => 1,
            Self::Panic => 2,
            Self::ParseFail(_) => 3,
            Self::CompileFail(_) => 4,
            Self::UnexpectedSuccess => 6,
            Self::TtxFail { .. } => 10,
            Self::CompareFail { .. } => 50,
        }
    }

    fn is_success(&self) -> bool {
        matches!(self, Self::Success)
    }

    /// Return an (optionally verbose) type for printing the result
    pub fn printer(&self, verbose: bool) -> impl std::fmt::Display + '_ {
        ReasonPrinter {
            reason: self,
            verbose,
        }
    }
}

impl std::fmt::Debug for ReportComparePrinter<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        debug_impl(f, self.new, Some(self.old), false)
    }
}

struct OldResults<'a> {
    map: Option<HashMap<&'a Path, TestResult>>,
}

impl<'a> OldResults<'a> {
    fn new(report: Option<&'a Report>) -> Self {
        Self {
            map: report.map(|report| {
                report
                    .results
                    .iter()
                    .map(|test| (test.path.as_path(), test.reason.clone()))
                    .collect()
            }),
        }
    }

    fn get(&self, result: &TestCase) -> ComparePrinter {
        match self.map.as_ref() {
            None => ComparePrinter::NotComparing,
            Some(map) => match map.get(result.path.as_path()) {
                None => ComparePrinter::Missing,
                Some(prev) => match (prev, &result.reason) {
                    (
                        TestResult::CompareFail {
                            diff_percent: old, ..
                        },
                        TestResult::CompareFail {
                            diff_percent: new, ..
                        },
                    ) => {
                        if (old - new).abs() > f64::EPSILON {
                            ComparePrinter::PercChange((new - old) * 100.)
                        } else {
                            ComparePrinter::Same
                        }
                    }
                    (x, y) if x == y => ComparePrinter::Same,
                    (old, _) => ComparePrinter::Different(old.clone()),
                },
            },
        }
    }
}

enum ComparePrinter {
    // print nothing, we aren't comparing
    NotComparing,
    // this item didn't previously exist
    Missing,
    // no diff
    Same,
    /// we are both compare failures, with a percentage change
    PercChange(f64),
    /// we are some other difference
    Different(TestResult),
}

impl std::fmt::Display for ComparePrinter {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            ComparePrinter::NotComparing => Ok(()),
            ComparePrinter::Missing => write!(f, "(new)"),
            ComparePrinter::Same => write!(f, "--"),
            ComparePrinter::PercChange(val) if val.is_sign_positive() => {
                write!(f, "{}", Color::Green.paint(format!("+{val:.2}")))
            }
            ComparePrinter::PercChange(val) => {
                write!(f, "{}", Color::Red.paint(format!("-{val:.2}")))
            }
            ComparePrinter::Different(reason) => write!(f, "{reason:?}"),
        }
    }
}

fn debug_impl(
    f: &mut std::fmt::Formatter,
    report: &Report,
    old: Option<&Report>,
    verbose: bool,
) -> std::fmt::Result {
    writeln!(f, "failed test cases")?;
    let path_pad = report.widest_path();
    let old_results = OldResults::new(old);

    for result in &report.results {
        let old = old_results.get(result);
        let file_name = result.path.file_name().unwrap().to_str().unwrap();
        writeln!(
            f,
            "{file_name:path_pad$}  {:<30}  {old}",
            result.reason.printer(verbose).to_string(),
        )?;
    }
    let summary = report.summary();
    let prefix = if old.is_some() { "new: " } else { "" };
    writeln!(f, "{prefix}{summary}")?;
    if let Some(old_summary) = old.map(Report::summary) {
        writeln!(f, "old: {old_summary}")?;
    }
    if !verbose {
        writeln!(f, "Set FEA_VERBOSE=1 for detailed output.")?;
    }

    Ok(())
}

impl std::fmt::Debug for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let verbose = std::env::var(super::VERBOSE).is_ok();
        debug_impl(f, self, None, verbose)
    }
}

impl Display for ReasonPrinter<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.reason {
            TestResult::Success => write!(f, "{}", Color::Green.paint("success")),
            TestResult::Panic => write!(f, "{}", Color::Red.paint("panic")),
            TestResult::ParseFail(diagnostics) => {
                write!(f, "{}", Color::Purple.paint("parse failure"))?;
                if self.verbose {
                    write!(f, "\n{}", diagnostics)?;
                }
                Ok(())
            }
            TestResult::CompileFail(diagnostics) => {
                write!(f, "{}", Color::Yellow.paint("compile failure"))?;
                if self.verbose {
                    write!(f, "\n{}", diagnostics)?;
                }
                Ok(())
            }
            TestResult::UnexpectedSuccess => {
                write!(f, "{}", Color::Yellow.paint("unexpected success"))
            }
            TestResult::TtxFail { code, std_err } => {
                write!(f, "ttx failure ({:?}) stderr:\n{}", code, std_err)
            }
            TestResult::CompareFail {
                expected,
                result,
                diff_percent,
            } => {
                if self.verbose {
                    writeln!(f, "compare failure")?;
                    super::write_line_diff(f, result, expected)
                } else {
                    write!(
                        f,
                        "{} ({:.0}%)",
                        Color::Blue.paint("compare failure"),
                        diff_percent * 100.0
                    )
                }
            }
        }
    }
}

impl Debug for TestResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.printer(std::env::var(super::VERBOSE).is_ok()).fmt(f)
    }
}

impl ReportSummary {
    fn total_items(&self) -> u32 {
        self.passed + self.panic + self.parse + self.compile + self.compare + self.other
    }

    fn average_diff_percent(&self) -> f64 {
        (self.sum_compare_perc + (self.passed as f64)) / self.total_items() as f64 * 100.
    }
}

impl Display for ReportSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let total = self.total_items();
        let perc = self.average_diff_percent();
        let ReportSummary {
            passed,
            panic,
            parse,
            compile,
            ..
        } = self;
        write!(f, "passed {passed}/{total} tests: ({panic} panics {parse} unparsed {compile} compile) {perc:.2}% avg diff")
    }
}
