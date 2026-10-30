use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
};

use eyre::{Context, OptionExt, Result, bail, ensure};
use tracing::{info, warn};

use super::{custom_args, mod_loader, platform};
use crate::{game::mod_loader::ModLoader, util::error::IoResultExt};

const BEPINEX_SCRIPT_CANDIDATES: &[&str] = &["start_game_bepinex.sh", "run_bepinex.sh"];

/// macOS modded BepInEx launch: resolve Steam exe from appinfo, find the
/// BepInEx start script, and run `script <exe> …` so the script can apply
/// `arch -e DYLD_*` to the game process (steam_osx cannot inject Doorstop).
pub fn create_modded_bepinex_command(
    game_dir: &Path,
    profile_dir: &Path,
    steam_app_id: Option<u32>,
    mod_loader_cfg: &ModLoader,
    game_custom_args: &str,
    profile_custom_args: &str,
) -> Result<Command> {
    let game_exe = resolve_game_executable(game_dir, steam_app_id)
        .context("failed to resolve game executable for macOS BepInEx launch")?;

    let script = find_bepinex_script(profile_dir, game_dir)
        .context("failed to find BepInEx start script")?;

    if let Err(err) = ensure_libdoorstop(game_dir) {
        warn!("failed to prepare macOS Doorstop files: {:#}", err);
    }

    let mut command = Command::new(&script);
    command.arg(&game_exe).current_dir(game_dir);

    if let Some(app_id) = steam_app_id {
        command.env("SteamAppId", app_id.to_string());
        command.env("SteamGameId", app_id.to_string());
    }

    let mut ctx = mod_loader::ArgsContext::new(&mut command, profile_dir, false);
    ctx.add_args(mod_loader_cfg)?;

    custom_args::add_args(&mut command, game_custom_args)?;
    custom_args::add_args(&mut command, profile_custom_args)?;

    info!(
        script = %script.display(),
        exe = %game_exe.display(),
        "using macOS BepInEx script launch"
    );

    Ok(command)
}

fn resolve_game_executable(game_dir: &Path, steam_app_id: Option<u32>) -> Result<PathBuf> {
    let Some(app_id) = steam_app_id else {
        bail!("macOS BepInEx launch requires a Steam app id to resolve the executable");
    };

    let raw = platform::get_steam_launch_options(app_id)
        .context("failed to read Steam launch options from appinfo")?;

    let options = raw
        .as_object()
        .ok_or_eyre("Steam launch options were not an object")?;

    let mut candidates = Vec::new();

    for option in options.values().filter_map(|v| v.as_object()) {
        if option
            .get("config")
            .and_then(|c| c.get("BetaKey"))
            .is_some()
        {
            continue;
        }

        let Some(executable) = option
            .get("executable")
            .and_then(|e| e.as_str())
            .map(|e| e.replace('\\', "/"))
            .filter(|e| !e.is_empty())
        else {
            continue;
        };

        let oslist = option
            .get("config")
            .and_then(|c| c.get("oslist"))
            .and_then(|o| o.as_str());

        let targets_macos = match oslist {
            None | Some("") => true,
            Some(list) => list.split(',').any(|os| os.trim() == "macos"),
        };
        if !targets_macos {
            continue;
        }

        let launch_type = option.get("type").and_then(|t| t.as_str());
        let is_default = launch_type == Some("default");
        candidates.push((is_default, executable));
    }

    ensure!(
        !candidates.is_empty(),
        "no macOS Steam launch executable found for app {}",
        app_id
    );

    candidates.sort_by_key(|(is_default, _)| if *is_default { 0 } else { 1 });
    let relative = &candidates[0].1;
    let path = materialize_steam_executable(game_dir, relative).with_context(|| {
        format!(
            "Steam launch executable {relative:?} not found under {}",
            game_dir.display()
        )
    })?;

    info!(
        app_id,
        path = %path.display(),
        "resolved Steam launch executable from appinfo"
    );

    Ok(path)
}

/// Join Steam's relative launch `executable` with the install dir.
///
/// appinfo is inconsistent on macOS:
/// - Valheim: `valheim.app`
/// - DEPO: `Contents/MacOS/DEPO` (relative to the `.app` / `.appbundle`)
fn materialize_steam_executable(game_dir: &Path, relative: &str) -> Result<PathBuf> {
    let direct = game_dir.join(relative);
    if direct.exists() {
        return Ok(direct);
    }

    // Path is inside an app bundle but Steam omitted the `Foo.app/` prefix.
    if relative.starts_with("Contents/") {
        let exe_name = Path::new(relative)
            .file_name()
            .map(|n| n.to_string_lossy().to_ascii_lowercase());
        let mut matches = Vec::new();

        for app in app_bundles_in(game_dir)? {
            let candidate = app.join(relative);
            if !candidate.exists() {
                continue;
            }

            let name_matches = exe_name.as_ref().is_some_and(|exe| {
                bundle_stem(&app)
                    .is_some_and(|stem| stem.eq_ignore_ascii_case(exe))
            });
            matches.push((name_matches, candidate));
        }

        matches.sort_by_key(|(name_matches, _)| if *name_matches { 0 } else { 1 });

        if let Some((_, path)) = matches.into_iter().next() {
            return Ok(path);
        }
    }

    bail!(
        "Steam launch executable does not exist at {} (also checked *.app|*.appbundle/{})",
        direct.display(),
        relative
    )
}

fn app_bundles_in(game_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut apps = Vec::new();
    for entry in fs::read_dir(game_dir).fs_context("reading game directory for app bundles", game_dir)?
    {
        let entry = entry.fs_context("reading game directory entry", game_dir)?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();

        if !path.is_dir() || !is_app_bundle_name(&name) {
            continue;
        }
        // Unity ships debug bundles next to the real app; skip those.
        if name.contains("BurstDebugInformation") || name.contains("DoNotShip") {
            continue;
        }

        apps.push(path);
    }
    Ok(apps)
}

fn is_app_bundle_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".app") || lower.ends_with(".appbundle")
}

/// Strip a trailing `.app` / `.appbundle` (any case) for name matching.
fn bundle_stem(app: &Path) -> Option<String> {
    let name = app.file_name()?.to_str()?;
    let lower = name.to_ascii_lowercase();
    let stem = if let Some(stripped) = lower.strip_suffix(".appbundle") {
        &name[..stripped.len()]
    } else if let Some(stripped) = lower.strip_suffix(".app") {
        &name[..stripped.len()]
    } else {
        return None;
    };
    Some(stem.to_string())
}

fn find_bepinex_script(profile_dir: &Path, game_dir: &Path) -> Result<PathBuf> {
    for dir in [profile_dir, game_dir] {
        for name in BEPINEX_SCRIPT_CANDIDATES {
            let path = dir.join(name);
            if path.is_file() {
                ensure_executable(&path)?;
                info!(path = %path.display(), "found BepInEx start script");
                return Ok(path);
            }
        }
    }

    bail!(
        "no BepInEx start script found in {} or {} (tried {})",
        profile_dir.display(),
        game_dir.display(),
        BEPINEX_SCRIPT_CANDIDATES.join(", ")
    )
}

fn ensure_executable(path: &Path) -> Result<()> {
    let mut perms = fs::metadata(path)
        .fs_context("reading BepInEx script metadata", path)?
        .permissions();
    let mode = perms.mode();
    if mode & 0o111 == 0 {
        perms.set_mode(mode | 0o755);
        fs::set_permissions(path, perms).fs_context("chmod +x BepInEx script", path)?;
    }
    Ok(())
}

fn ensure_libdoorstop(game_dir: &Path) -> Result<()> {
    let dest = game_dir.join("libdoorstop.dylib");
    if dest.exists() {
        return Ok(());
    }

    let from_libs = game_dir
        .join("doorstop_libs")
        .join("libdoorstop_x64.dylib");

    ensure!(
        from_libs.exists(),
        "libdoorstop.dylib missing and doorstop_libs/libdoorstop_x64.dylib not found in {}",
        game_dir.display()
    );

    info!(
        from = %from_libs.display(),
        to = %dest.display(),
        "copying doorstop dylib for macOS launch"
    );
    fs::copy(&from_libs, &dest).fs_context("copying libdoorstop.dylib", &from_libs)?;
    Ok(())
}
