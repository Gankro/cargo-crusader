extern crate curl;
extern crate env_logger;
#[macro_use]
extern crate log;
extern crate rustc_serialize;
extern crate semver;
extern crate toml;
extern crate threadpool;
extern crate num_cpus;

use curl::{http, ErrCode};
use curl::http::Response as CurlHttpResponse;
use rustc_serialize::json;
use semver::Version;
use std::convert::From;
use std::env;
use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::path::{PathBuf, Path};
use std::str::{self, Utf8Error};
use std::sync::mpsc::{self, Sender, Receiver, RecvError};
use threadpool::ThreadPool;

fn main() {
    env_logger::init().unwrap();
    report_results(run());
}

fn run() -> Result<Vec<TestResult>, Error> {
    let config = try!(get_config());

    let rev_deps = try!(get_rev_deps(&config.crate_name));
    let crates = try!(acquire_crates(&config));
    let mut results = Vec::new();
    let ref mut pool = ThreadPool::new(num_cpus::get());
    for rev_dep in rev_deps {
        let result = run_test(pool, crates.base.clone(), crates.next.clone(), rev_dep);
        results.push(result);
    }

    let results = results.into_iter().map(|r| r.take());
    let results = results.collect::<Vec<_>>();
    let failed = results.iter().any(|r| r.failed());

    if failed {
        Err(Error::TestFailure(results))
    } else {
        Ok(results)
    }
}

struct Config {
    manifest_path: PathBuf,
    crate_name: String,
    base_origin: Origin,
    next_origin: Origin
}

enum Origin {
    Published,
    Source(PathBuf)
}

type VersionNumber = String;

fn get_config() -> Result<Config, Error> {
    let manifest = env::var("CRUSADER_MANIFEST");
    let manifest = manifest.unwrap_or_else(|_| "./Cargo.toml".to_string());
    let manifest = PathBuf::from(manifest);
    info!("Using manifest {:?}", manifest);

    let source_name = try!(get_crate_name(&manifest));
    Ok(Config {
        manifest_path: manifest.clone(),
        crate_name: source_name,
        base_origin: Origin::Published,
        next_origin: Origin::Source(manifest)
    })
}

fn get_crate_name(manifest_path: &Path) -> Result<String, Error> {
    let ref toml = try!(load_string(manifest_path));
    let mut parser = toml::Parser::new(toml);
    let toml = parser.parse();
    let map = if toml.is_none() {
        return Err(Error::TomlError(parser.errors))
    } else {
        toml.unwrap()
    };

    match map.get("package") {
        Some(&toml::Value::Table(ref t)) => {
            match t.get("name") {
                Some(&toml::Value::String(ref s)) => {
                    Ok(s.clone())
                }
                _ => {
                    Err(Error::ManifestName(PathBuf::from(manifest_path)))
                }
            }
        }
        _ => {
            Err(Error::ManifestName(PathBuf::from(manifest_path)))
        }
    }
}

fn load_string(path: &Path) -> Result<String, Error> {
    let mut file = try!(File::open(path));
    let mut s = String::new();
    try!(file.read_to_string(&mut s));
    Ok(s)
}

type RevDepName = String;

fn crate_url(krate: &str, call: Option<&str>) -> String {
    let url = format!("https://crates.io/api/v1/crates/{}", krate);
    match call {
        Some(c) => format!("{}/{}", url, c),
        None => url
    }
}

fn get_rev_deps(crate_name: &str) -> Result<Vec<RevDepName>, Error> {
    info!("Getting reverse deps for {}", crate_name);
    let ref url = crate_url(crate_name, Some("reverse_dependencies"));
    let ref body = try!(http_get_to_string(url));
    let rev_deps = try!(parse_rev_deps(body));

    Ok(rev_deps)
}

fn http_get_to_string(url: &str) -> Result<String, Error> {
    let resp = try!(http::handle().get(url).exec());

    if resp.get_code() != 200 {
        return Err(Error::HttpError(CurlHttpResponseWrapper(resp)));
    }

    let body = try!(str::from_utf8(resp.get_body()));

    Ok(String::from(body))
}

fn parse_rev_deps(s: &str) -> Result<Vec<RevDepName>, Error> {
    #[derive(RustcEncodable, RustcDecodable)]
    struct Response {
        dependencies: Vec<Dep>,
    }

    #[derive(RustcEncodable, RustcDecodable)]
    struct Dep {
        crate_id: String
    }

    let decoded: Response = try!(json::decode(&s));

    fn depconv(d: Dep) -> RevDepName { d.crate_id }

    let revdeps = decoded.dependencies.into_iter()
        .map(depconv).collect();

    info!("revdeps: {:?}", revdeps);

    Ok(revdeps)
}

struct Crates {
    base: CrateOverride,
    next: CrateOverride
}

#[derive(Clone)]
enum CrateOverride {
    Default,
    Source(PathBuf)
}

fn acquire_crates(config: &Config) -> Result<Crates, Error> {
    let base = acquire_crate(&config.base_origin);
    let next = acquire_crate(&config.next_origin);
    Ok(Crates { base: base, next: next })
}

fn acquire_crate(origin: &Origin) -> CrateOverride {
    match *origin {
        Origin::Published => CrateOverride::Default,
        Origin::Source(ref p) => CrateOverride::Source(p.clone())
    }
}

#[derive(Debug, Clone)]
struct RevDep {
    name: RevDepName,
    vers: Version
}

#[derive(Debug)]
struct TestResult {
    rev_dep: RevDep,
    data: TestResultData
}

#[derive(Debug)]
enum TestResultData {
    Broken(CompileResult),
    Fail(CompileResult, CompileResult),
    Pass(CompileResult, CompileResult),
    Error(Error),
}

impl TestResult {
    fn broken(rev_dep: RevDep, r: CompileResult) -> TestResult {
        TestResult {
            rev_dep: rev_dep,
            data: TestResultData::Broken(r)
        }
    }

    fn fail(rev_dep: RevDep, r1: CompileResult, r2: CompileResult) -> TestResult {
        TestResult {
            rev_dep: rev_dep,
            data: TestResultData::Fail(r1, r2)
        }
    }

    fn pass(rev_dep: RevDep, r1: CompileResult, r2: CompileResult) -> TestResult {
        TestResult {
            rev_dep: rev_dep,
            data: TestResultData::Pass(r1, r2)
        }
    }

    fn error(rev_dep: RevDep, e: Error) -> TestResult {
        TestResult {
            rev_dep: rev_dep,
            data: TestResultData::Error(e)
        }
    }
    
    fn failed(&self) -> bool {
        match self.data {
            TestResultData::Fail(..) => true,
            _ => false
        }
    }
}

struct TestResultFuture {
    rev_dep: RevDepName,
    rx: Receiver<TestResult>
}

impl TestResultFuture {
    fn take(self) -> TestResult {
        match self.rx.recv() {
            Ok(r) => r,
            Err(e) => {
                let r = RevDep {
                    name: self.rev_dep,
                    vers: Version::parse("0.0.0").unwrap()
                };
                TestResult::error(r, Error::from(e))
            }
        }
    }
}

fn new_result_future(rev_dep: RevDepName) -> (Sender<TestResult>, TestResultFuture) {
    let (tx, rx) = mpsc::channel();

    let fut = TestResultFuture {
        rev_dep: rev_dep,
        rx: rx
    };

    (tx, fut)
}

fn run_test(pool: &mut ThreadPool,
            base_crate: CrateOverride,
            next_crate: CrateOverride,
            rev_dep: RevDepName) -> TestResultFuture {
    let (result_tx, result_future) = new_result_future(rev_dep.clone());
    pool.execute(move || {
        let res = run_test_local(&base_crate, &next_crate, rev_dep);
        result_tx.send(res);
    });

    return result_future;
}

fn run_test_local(base_crate: &CrateOverride, next_crate: &CrateOverride, rev_dep: RevDepName) -> TestResult {
    let rev_dep = match resolve_rev_dep_version(rev_dep.clone()) {
        Ok(r) => r,
        Err(e) => {
            let rev_dep = RevDep {
                name: rev_dep,
                vers: Version::parse("0.0.0").unwrap()
            };
            return TestResult::error(rev_dep, e);
        }
    };
    let base_result = compile_with_custom_dep(&rev_dep, base_crate);

    if base_result.failed() {
        return TestResult::broken(rev_dep, base_result);
    }
    let next_result = compile_with_custom_dep(&rev_dep, next_crate);

    if next_result.failed() {
        TestResult::fail(rev_dep, base_result, next_result)
    } else {
        TestResult::pass(rev_dep, base_result, next_result)
    }
}

fn resolve_rev_dep_version(name: RevDepName) -> Result<RevDep, Error> {
    info!("resolving current version for {}", name);
    let ref url = crate_url(&name, None);
    let ref body = try!(http_get_to_string(url));
    let krate = try!(parse_crate(body));
    println!("{:?}", krate);
    unimplemented!()
}

#[derive(RustcEncodable, RustcDecodable, Debug)]
struct RegistryCrate {
    versions: Vec<RegistryVersion>
}

#[derive(RustcEncodable, RustcDecodable, Debug)]
struct RegistryVersion {
    num: String
}

fn parse_crate(s: &str) -> Result<RegistryCrate, Error> {
    Ok(try!(json::decode(&s)))
}

#[derive(Debug, Clone)]
struct CompileResult {
    stdout: String,
    stderr: String,
    success: bool
}

impl CompileResult {
    fn failed(&self) -> bool { unimplemented!() }
}

fn compile_with_custom_dep(rev_dep: &RevDep, krate: &CrateOverride) -> CompileResult {
    //let temp_dir = get_temp_dir();
    //let crate_handle = get_crate_handle(rev_dep);

    
    unimplemented!()
}

fn report_results(res: Result<Vec<TestResult>, Error>) {
    println!("results: {:?}", res);
}

#[derive(Debug)]
enum Error {
    BadArgs,
    ManifestName(PathBuf),
    TestFailure(Vec<TestResult>),
    SemverError(semver::ParseError),
    TomlError(Vec<toml::ParserError>),
    IoError(io::Error),
    CurlError(curl::ErrCode),
    HttpError(CurlHttpResponseWrapper),
    Utf8Error(Utf8Error),
    JsonDecode(json::DecoderError),
    RecvError(RecvError)
}

impl From<semver::ParseError> for Error {
    fn from(e: semver::ParseError) -> Error {
        Error::SemverError(e)
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        Error::IoError(e)
    }
}

impl From<curl::ErrCode> for Error {
    fn from(e: curl::ErrCode) -> Error {
        Error::CurlError(e)
    }
}

impl From<Utf8Error> for Error {
    fn from(e: Utf8Error) -> Error {
        Error::Utf8Error(e)
    }
}

impl From<json::DecoderError> for Error {
    fn from(e: json::DecoderError) -> Error {
        Error::JsonDecode(e)
    }
}

impl From<RecvError> for Error {
    fn from(e: RecvError) -> Error {
        Error::RecvError(e)
    }
}

struct CurlHttpResponseWrapper(CurlHttpResponse);

impl fmt::Debug for CurlHttpResponseWrapper {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        let CurlHttpResponseWrapper(ref resp) = *self;
        let tup = (resp.get_code(), resp.get_headers(), resp.get_body());
        try!(fmt.write_str(&format!("{:?}", tup)));

        Ok(())
    }
}
