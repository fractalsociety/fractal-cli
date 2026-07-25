//! Specialized native-iOS and cross-platform mobile build profiles.
//!
//! These commands generate a bounded PRD for the existing Claude-led execution
//! graph, then verify with the platform's real toolchain and optionally launch
//! the result. They deliberately avoid the generic Python greenfield contract.

use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use crate::cli::{IosArgs, MobileArgs, MobileFramework, MobilePlatform, DEFAULT_GRAPH_PORT};

const DEFAULT_AGENTS: &str = "claude,cursor,codex";
const IOS_PRD: &str = "FRACTAL_IOS_APP_PRD.md";
const MOBILE_PRD: &str = "FRACTAL_MOBILE_APP_PRD.md";

pub(crate) fn run_ios(
    args: &IosArgs,
    fractalwork_override: Option<&Path>,
    coordinate: bool,
) -> Result<()> {
    let slug = project_slug(&args.request);
    let app_name = swift_name(&slug);
    let workspace = project_workspace(args.repo.as_deref(), &format!("{slug}-ios"))?;
    let prd = ios_prd(&args.request, &app_name, &slug);

    if args.dry_run {
        print_plan("ios-swiftui", &workspace, &app_name, &prd, args.launch);
        return Ok(());
    }

    require_tool("xcodebuild", "Install Xcode from the Mac App Store.")?;
    require_tool("xcrun", "Install Xcode and its command-line components.")?;
    require_tool("xcodegen", "Run `brew install xcodegen`.")?;
    prepare_workspace(&workspace, IOS_PRD, &prd, "ios-swiftui")?;
    configure_agents();
    std::env::set_var("FRACTAL_IOS_SIMULATOR", &args.simulator);

    let request = format!(
        "Execute {IOS_PRD} completely. Build the native SwiftUI app described by the user: {}",
        args.request
    );
    let outcome = crate::interactive::execute_ingested(
        &request,
        Some(&workspace),
        fractalwork_override,
        coordinate,
        DEFAULT_GRAPH_PORT,
    )?;

    // Only launch a COMPLETE, non-failed build — never an unfinished app. A run
    // that stopped early (a task failed, or evolution gave up) leaves a resumable
    // checkpoint; launching it would ship a half-built app.
    if !build_succeeded(&outcome) {
        report_incomplete(&outcome, &workspace);
        return Ok(());
    }

    if args.launch {
        launch_native_ios(&workspace, &args.simulator)?;
    }
    Ok(())
}

/// A build is launchable only when every task ran with no failure (the verify node
/// may be unverifiable for a native toolchain, which is not a failure).
fn build_succeeded(outcome: &Option<crate::execute::RunOutcome>) -> bool {
    outcome
        .as_ref()
        .is_some_and(|o| o.built && o.verified != Some(false) && o.failed_node.is_none())
}

/// Explain why an incomplete build was not launched and how to finish it.
fn report_incomplete(outcome: &Option<crate::execute::RunOutcome>, workspace: &Path) {
    let detail = outcome
        .as_ref()
        .map(|o| o.detail.clone())
        .unwrap_or_else(|| "the build did not run".to_owned());
    println!();
    println!("⚠  The app build did not complete — {detail}.");
    println!("   NOT launching an unfinished app.");
    println!(
        "   The live board (http://127.0.0.1:{DEFAULT_GRAPH_PORT}) shows which tasks are done; re-run"
    );
    println!(
        "   `fractal ios …` (or `fractal` in {}) to resume from the checkpoint, then launch.",
        workspace.display()
    );
}

pub(crate) fn run_mobile(
    args: &MobileArgs,
    fractalwork_override: Option<&Path>,
    coordinate: bool,
) -> Result<()> {
    if args.platforms.is_empty() {
        bail!("--platforms must contain ios, android, or both");
    }
    if let Some(launch) = args.launch {
        if !args.platforms.contains(&launch) {
            bail!("--launch {launch} is not included in --platforms");
        }
    }

    let slug = project_slug(&args.request);
    let app_name = swift_name(&slug);
    let workspace = project_workspace(args.repo.as_deref(), &format!("{slug}-mobile"))?;
    let platforms = args
        .platforms
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let prd = expo_prd(&args.request, &app_name, &slug, &platforms);

    if args.dry_run {
        print_plan(
            &format!("mobile-{}", args.framework),
            &workspace,
            &app_name,
            &prd,
            args.launch.is_some(),
        );
        return Ok(());
    }

    match args.framework {
        MobileFramework::Expo => {
            require_tool("node", "Install Node.js 20 or newer.")?;
            require_tool("npm", "Install Node.js 20 or newer.")?;
            require_tool("npx", "Install npm/npx.")?;
        }
    }
    if args.platforms.contains(&MobilePlatform::Ios) {
        require_tool("xcodebuild", "Install Xcode from the Mac App Store.")?;
    }

    prepare_workspace(&workspace, MOBILE_PRD, &prd, "mobile-expo")?;
    configure_agents();
    std::env::set_var("FRACTAL_IOS_SIMULATOR", &args.simulator);
    let request = format!(
        "Execute {MOBILE_PRD} completely. Build the {} app for {platforms}: {}",
        args.framework, args.request
    );
    let outcome = crate::interactive::execute_ingested(
        &request,
        Some(&workspace),
        fractalwork_override,
        coordinate,
        DEFAULT_GRAPH_PORT,
    )?;

    // Never launch an unfinished build (see run_ios).
    if !build_succeeded(&outcome) {
        report_incomplete(&outcome, &workspace);
        return Ok(());
    }

    match args.launch {
        Some(MobilePlatform::Ios) => launch_expo(&workspace, "ios", &args.simulator)?,
        Some(MobilePlatform::Android) => launch_expo(&workspace, "android", "")?,
        None => {}
    }
    Ok(())
}

fn print_plan(profile: &str, workspace: &Path, app_name: &str, prd: &str, launch: bool) {
    println!("Profile: {profile}");
    println!("Workspace: {}", workspace.display());
    println!("App name: {app_name}");
    println!("Agents: {DEFAULT_AGENTS} (lead first)");
    println!("Launch after verification: {launch}");
    println!("\nSpecialized PRD:\n{prd}");
}

fn configure_agents() {
    std::env::remove_var("FRACTAL_WORKER");
    std::env::set_var("FRACTAL_AGENTS", DEFAULT_AGENTS);
}

fn project_workspace(override_path: Option<&Path>, slug: &str) -> Result<PathBuf> {
    if let Some(path) = override_path {
        return Ok(if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        });
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set; pass --repo explicitly")?;
    Ok(home.join("fractal-projects").join(slug))
}

fn prepare_workspace(workspace: &Path, prd_name: &str, prd: &str, profile: &str) -> Result<()> {
    fs::create_dir_all(workspace)
        .with_context(|| format!("create project workspace {}", workspace.display()))?;
    let workspace = workspace
        .canonicalize()
        .with_context(|| format!("resolve project workspace {}", workspace.display()))?;
    fs::write(workspace.join(prd_name), prd)
        .with_context(|| format!("write specialized PRD {prd_name}"))?;
    fs::write(workspace.join(".fractal-profile"), format!("{profile}\n"))
        .context("write mobile profile marker")?;
    trust_workspace(&workspace)?;
    Ok(())
}

fn trust_workspace(workspace: &Path) -> Result<()> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")?;
    let store = home.join(".fractal").join("trusted-folders.txt");
    if let Some(parent) = store.parent() {
        fs::create_dir_all(parent)?;
    }
    let canonical = workspace.display().to_string();
    let existing = fs::read_to_string(&store).unwrap_or_default();
    if existing.lines().any(|line| line.trim() == canonical) {
        return Ok(());
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&store)
        .with_context(|| format!("open trust store {}", store.display()))?;
    writeln!(file, "{canonical}")?;
    Ok(())
}

fn require_tool(tool: &str, remedy: &str) -> Result<()> {
    let available = std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|dir| {
                let candidate = dir.join(tool);
                candidate.is_file()
            })
        })
        .unwrap_or(false);
    if !available {
        bail!("required tool `{tool}` is not on PATH. {remedy}");
    }
    Ok(())
}

fn ios_prd(request: &str, app_name: &str, slug: &str) -> String {
    let bundle = slug.replace('-', "");
    format!(
        r#"# Native iOS application

## User request

{request}

## Fixed build contract

- Build a native Swift 6 + SwiftUI application named `{app_name}`.
- Target iOS 17 or newer and use Apple frameworks; do not create Python,
  React Native, Flutter, web, or command-line substitutes.
- Use an MVVM-style feature structure with small testable models and services.
- Persist user data locally with SwiftData unless the request requires another
  Apple-native store.
- Provide accessibility labels, empty/error/loading states, previews, and
  deterministic sample data where relevant.
- Create `project.yml` for XcodeGen, one application target named `{app_name}`,
  bundle id `com.fractal.{bundle}`, and a shared `{app_name}` scheme.
- Put application sources under `{app_name}/` and tests under
  `{app_name}Tests/`.
- Add meaningful XCTest or Swift Testing unit tests for the primary behavior.
- Generate the project with `xcodegen generate`.
- Run:
  `xcodebuild test -project {app_name}.xcodeproj -scheme {app_name} -destination "platform=iOS Simulator,name=iPhone 17 Pro" CODE_SIGNING_ALLOWED=NO`
- Fix every compiler error and failing test. Do not claim completion merely
  because files exist.

## Team decomposition

Claude owns architecture, `APP_SPEC.md`, interfaces, and final integration.
Cursor and Codex independently implement ready UI/model/test tasks. Keep tasks
dependency-aware so implementation and tests can proceed in parallel after the
architecture is fixed. The final graph node must execute the real Xcode test
suite.
"#
    )
}

fn expo_prd(request: &str, app_name: &str, slug: &str, platforms: &str) -> String {
    format!(
        r#"# Expo cross-platform mobile application

## User request

{request}

## Fixed build contract

- Build an Expo + React Native + TypeScript application named `{app_name}` for
  `{platforms}`. Do not create a web-only, Python, Flutter, or native-Swift-only
  substitute.
- Use the current stable Expo SDK and an app slug of `{slug}`.
- Keep shared product logic and UI in TypeScript. Isolate platform-specific code
  only where behavior genuinely differs.
- Create `package.json`, `app.json`, `tsconfig.json`, application sources, assets,
  and tests directly in this workspace.
- Include deterministic `npm test`, `npm run typecheck`, and `npm run lint`
  scripts that work non-interactively.
- Include `npm run fractal:verify`, chaining those three checks, and install all
  declared dependencies before the verification node runs.
- Add meaningful tests for the primary state and user flows.
- Run all three validation scripts and fix every failure.
- Do not configure EAS credentials, signing, store submission, analytics, or
  production deployment.

## Team decomposition

Claude owns architecture, `APP_SPEC.md`, navigation/data contracts, and final
integration. Cursor and Codex implement ready UI/state/test tasks in parallel.
The final graph node must execute the real npm test suite.
"#
    )
}

fn project_slug(request: &str) -> String {
    let mut words = Vec::new();
    for raw in request.split_whitespace() {
        let word = raw
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();
        if word.is_empty()
            || matches!(
                word.as_str(),
                "build"
                    | "create"
                    | "make"
                    | "me"
                    | "a"
                    | "an"
                    | "the"
                    | "new"
                    | "ios"
                    | "mobile"
                    | "app"
                    | "application"
                    | "please"
            )
        {
            continue;
        }
        words.push(word);
        if words.len() == 6 {
            break;
        }
    }
    let slug = words.join("-");
    if slug.is_empty() {
        "fractal-app".to_owned()
    } else {
        slug
    }
}

fn swift_name(slug: &str) -> String {
    let mut name = String::new();
    for word in slug.split('-') {
        let mut chars = word.chars();
        if let Some(first) = chars.next() {
            name.extend(first.to_uppercase());
            name.extend(chars);
        }
    }
    if name.is_empty() {
        "FractalApp".to_owned()
    } else if name.as_bytes()[0].is_ascii_digit() {
        format!("App{name}")
    } else {
        name
    }
}

fn launch_native_ios(workspace: &Path, simulator: &str) -> Result<()> {
    run_checked(
        Command::new("xcodegen")
            .arg("generate")
            .current_dir(workspace),
        "generate the Xcode project",
    )?;
    let project = first_with_extension(workspace, "xcodeproj")
        .context("XcodeGen completed but no .xcodeproj was produced")?;
    let scheme = project
        .file_stem()
        .and_then(OsStr::to_str)
        .context("Xcode project has no valid scheme name")?;

    let _ = Command::new("open").args(["-a", "Simulator"]).status();
    let _ = Command::new("xcrun")
        .args(["simctl", "boot", simulator])
        .status();
    run_checked(
        Command::new("xcrun")
            .args(["simctl", "bootstatus", simulator, "-b"])
            .current_dir(workspace),
        "wait for iOS Simulator",
    )?;

    let derived = workspace.join(".fractal").join("DerivedData");
    run_checked(
        Command::new("xcodebuild")
            .arg("-project")
            .arg(&project)
            .args(["-scheme", scheme])
            .arg("-destination")
            .arg(format!("platform=iOS Simulator,name={simulator}"))
            .arg("-derivedDataPath")
            .arg(&derived)
            .args(["CODE_SIGNING_ALLOWED=NO", "build"])
            .current_dir(workspace),
        "build the iOS app for Simulator",
    )?;

    let products = derived.join("Build").join("Products");
    let expected_app = products
        .join("Debug-iphonesimulator")
        .join(format!("{scheme}.app"));
    let app = if expected_app.is_dir() {
        expected_app
    } else {
        find_bundle(&products, "app").context("built iOS .app bundle was not found")?
    };
    run_checked(
        Command::new("xcrun")
            .args(["simctl", "install", "booted"])
            .arg(&app),
        "install the app in Simulator",
    )?;
    let plist = app.join("Info.plist");
    let output = Command::new("/usr/libexec/PlistBuddy")
        .args(["-c", "Print :CFBundleIdentifier"])
        .arg(&plist)
        .output()
        .context("read the built app bundle identifier")?;
    if !output.status.success() {
        bail!("could not read CFBundleIdentifier from {}", plist.display());
    }
    let bundle_id = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    run_checked(
        Command::new("xcrun").args(["simctl", "launch", "booted", &bundle_id]),
        "launch the app in Simulator",
    )?;
    println!("✓ Launched {scheme} on {simulator}");
    Ok(())
}

fn launch_expo(workspace: &Path, platform: &str, simulator: &str) -> Result<()> {
    run_checked(
        Command::new("npm").arg("install").current_dir(workspace),
        "install Expo dependencies",
    )?;
    let mut command = Command::new("npx");
    command.args(["expo", &format!("run:{platform}")]);
    if platform == "ios" {
        command.args(["--device", simulator]);
    }
    run_checked(
        command.current_dir(workspace),
        &format!("build and launch Expo on {platform}"),
    )?;
    println!("✓ Launched Expo app on {platform}");
    Ok(())
}

fn run_checked(command: &mut Command, action: &str) -> Result<()> {
    command.stdin(Stdio::null());
    let status = command
        .status()
        .with_context(|| format!("failed to {action}"))?;
    if !status.success() {
        bail!("{action} failed with {status}");
    }
    Ok(())
}

fn first_with_extension(root: &Path, extension: &str) -> Option<PathBuf> {
    fs::read_dir(root)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.extension() == Some(OsStr::new(extension)))
}

fn find_bundle(root: &Path, extension: &str) -> Option<PathBuf> {
    for entry in fs::read_dir(root).ok()?.flatten() {
        let path = entry.path();
        if path.extension() == Some(OsStr::new(extension)) {
            return Some(path);
        }
        if path.is_dir() {
            if let Some(found) = find_bundle(&path, extension) {
                return Some(found);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_stable_project_names() {
        let slug = project_slug("Build a personal expense tracker iOS app");
        assert_eq!(slug, "personal-expense-tracker");
        assert_eq!(swift_name(&slug), "PersonalExpenseTracker");
    }

    #[test]
    fn native_prd_is_not_python_greenfield_work() {
        let prd = ios_prd("Build a tracker", "Tracker", "tracker");
        assert!(prd.contains("SwiftUI"));
        assert!(prd.contains("xcodebuild test"));
        assert!(prd.contains("do not create Python"));
    }

    #[test]
    fn expo_prd_has_noninteractive_quality_gates() {
        let prd = expo_prd("Build a tracker", "Tracker", "tracker", "ios,android");
        assert!(prd.contains("Expo + React Native + TypeScript"));
        assert!(prd.contains("npm run typecheck"));
        assert!(prd.contains("ios,android"));
    }
}
