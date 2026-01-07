// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::{
  collections::HashMap,
  fs,
  path::{Path, PathBuf},
  process::Command,
};

use anyhow::Context;

use crate::{
  bundle::{linux::debian, settings::Arch},
  utils::{fs_utils, http_utils::download, CommandExt},
  Settings,
};

use super::write_and_make_executable;

// TODO: Test if bundling xdg-mime makes sense (eg does it even work if it's not on the host system?)
// TODO: Monitor TLS support / certificates - seems to be working in initial tests
pub fn bundle_project(settings: &Settings) -> crate::Result<Vec<PathBuf>> {
  // for backwards compat we keep the amd64 and i386 rewrites in the filename
  let appimage_arch = match settings.binary_arch() {
    Arch::X86_64 => "amd64",
    //Arch::X86 => "i386",
    Arch::AArch64 => "aarch64",
    //Arch::Armhf => "armhf",
    target => {
      return Err(crate::Error::ArchError(format!(
        "Unsupported architecture: {target:?}"
      )));
    }
  };
  let tools_arch = settings.target().split('-').next().unwrap();

  let output_path = settings.project_out_directory().join("bundle/appimage");
  if output_path.exists() {
    fs::remove_dir_all(&output_path)?;
  }

  let tools_path = settings
    .local_tools_directory()
    .map(|d| d.join(".tauri"))
    .unwrap_or_else(|| {
      dirs::cache_dir().map_or_else(|| output_path.to_path_buf(), |p| p.join("tauri"))
    });

  fs::create_dir_all(&tools_path)?;

  let (sharun_aio, uruntime, uruntime_lite) =
    prepare_tools(&tools_path, tools_arch, settings.appimage().squashfs)?;

  let package_dir = settings
    .project_out_directory()
    .join("bundle/appimage_deb/");

  let main_binary = settings.main_binary()?;
  let product_name = settings.product_name();

  let mut settings = settings.clone();
  if main_binary.name().contains(' ') {
    let main_binary_path = settings.binary_path(main_binary);
    let project_out_dir = settings.project_out_directory();

    let main_binary_name_kebab = heck::AsKebabCase(main_binary.name()).to_string();
    let new_path = project_out_dir.join(&main_binary_name_kebab);
    fs::copy(main_binary_path, new_path)?;

    let main_binary = settings.main_binary_mut()?;
    main_binary.set_name(main_binary_name_kebab);
  }

  let upinfo = std::env::var("UPINFO")
    .ok()
    .or(settings.appimage().update_information.clone());

  // generate deb_folder structure
  let (data_dir, icons) = debian::generate_data(&settings, &package_dir)
    .with_context(|| "Failed to build data folders and files")?;
  fs_utils::copy_custom_files(&settings.appimage().files, &data_dir)
    .with_context(|| "Failed to copy custom files")?;

  fs::create_dir_all(&output_path)?;
  let app_dir_path = output_path.join(format!("{}.AppDir", settings.product_name()));
  let appimage_filename = format!(
    "{}_{}_{appimage_arch}.AppImage",
    settings.product_name(),
    settings.version_string()
  );
  let appimage_path = output_path.join(&appimage_filename);

  fs::create_dir_all(&tools_path)?;
  let larger_icon = icons
    .iter()
    .filter(|i| i.width == i.height)
    .max_by_key(|i| i.width)
    .expect("couldn't find a square icon to use as AppImage icon");
  let larger_icon_path = larger_icon
    .path
    .strip_prefix(package_dir.join("data"))
    .unwrap()
    .to_string_lossy()
    .to_string();

  log::info!(action = "Bundling"; "{} ({})", appimage_filename, appimage_path.display());

  fs_utils::copy_dir(&data_dir, &app_dir_path)?;

  let app_dir_share = &app_dir_path.join("share/");

  fs_utils::copy_dir(&data_dir.join("usr/share/"), app_dir_share)?;

  // The appimage spec allows a symlink but sharun doesn't
  fs::copy(
    app_dir_share.join(format!("applications/{product_name}.desktop")),
    app_dir_path.join(format!("{product_name}.desktop")),
  )?;

  // This could be a symlink as well (supported by sharun as far as i can tell)
  fs::copy(
    app_dir_path.join(larger_icon_path.strip_prefix("usr/").unwrap()),
    app_dir_path.join(format!("{product_name}.png")),
  )?;

  std::os::unix::fs::symlink(
    app_dir_path.join(format!("{product_name}.png")),
    app_dir_path.join(".DirIcon"),
  )?;

  let verbosity = match settings.log_level() {
    log::Level::Error => "-q", // errors only
    log::Level::Info => "",    // errors + "normal logs" (mostly rpath)
    log::Level::Trace => "-v", // You can expect way over 1k lines from just lib4bin on this level
    _ => "",
  };

  // TODO: Maybe missing alsa, pipewire, whatever?
  // TODO: Test on fedora & arch, currently errors out on Ubuntu
  let gst = if settings.appimage().bundle_media_framework {
    format!(
      r#"
/usr/lib/{tools_arch}-linux-gnu/libpulsecommon* \
/usr/lib/{tools_arch}-linux-gnu/gstreamer-1.0/* \
/usr/lib/{tools_arch}-linux-gnu/gstreamer1.0/gstreamer-1.0/* \
"#
    )
  } else {
    "".to_string()
  };

  // Build map of all sidecar binaries to preserve unstripped
  // This ensures byte-equivalence for binaries with appended data (e.g., Bun-compiled executables)
  // Key: binary_name, Value: original_source_path
  let mut sidecars_to_preserve: HashMap<String, PathBuf> = HashMap::new();

  for src in settings.external_binaries() {
    let src = src?;
    let src_filename = src
      .file_name()
      .expect("failed to extract external binary filename")
      .to_string_lossy();

    // Remove target triple suffix (same logic as copy_binaries)
    let binary_name = src_filename.replace(&format!("-{}", settings.target()), "");

    sidecars_to_preserve.insert(binary_name.clone(), src.to_path_buf());
  }

  if !sidecars_to_preserve.is_empty() {
    for (name, path) in &sidecars_to_preserve {
      log::info!(
        "Will preserve unstripped sidecar: {} (source: {})",
        name,
        path.display()
      );
    }
  }

  let bins = settings.copy_binaries(&app_dir_path.join("usr/bin/"))?;
  let bins = bins
    .iter()
    .map(|b| format!(" \"{}\"", b.to_string_lossy()))
    .collect::<String>();

  let xvfb = if which::which("xvfb-run").is_ok() {
    "xvfb-run -a -- "
  } else {
    log::warn!("xvfb-run not found but heavily recommended! In headless mode the bundler will likely miss some required libraries.");
    ""
  };

  // TODO: Check if we can make parts of the opengl (incl. libvulkan) deps optional
  Command::new("/bin/sh")
    .current_dir(&app_dir_path)
    .args([
      "-c",
      &format!(
        r#"{}"{}" l -p {verbosity} -e -s -k "{}" {} \
/usr/lib/{tools_arch}-linux-gnu/libwebkit2gtk-4.1* \{gst}
/usr/lib/{tools_arch}-linux-gnu/gdk-pixbuf-*/*/*/* \
/usr/lib/{tools_arch}-linux-gnu/gio/modules/* \
/usr/lib/{tools_arch}-linux-gnu/libnss*.so* \
/usr/lib/{tools_arch}-linux-gnu/libGL* \
/usr/lib/{tools_arch}-linux-gnu/libEGL* \
/usr/lib/{tools_arch}-linux-gnu/libvulkan* \
/usr/lib/{tools_arch}-linux-gnu/dri/* \
/usr/lib/{tools_arch}-linux-gnu/gbm/*
"#,
        xvfb,
        sharun_aio.to_string_lossy(),
        &app_dir_path
          .join(format!("usr/bin/{}", main_binary.name()))
          .to_string_lossy(),
        bins
      ),
    ])
    .output_ok()
    .context("lib4bin command failed to run.")?;

  // Sharun has completed processing with stripping enabled
  // Restore original unstripped versions of all sidecar binaries to preserve byte-equivalence
  // (Main binary stays stripped since it's a standard Rust executable)
  if !sidecars_to_preserve.is_empty() {
    let bin_dir = app_dir_path.join("bin");

    log::info!("Looking for stripped sidecars in: {}", bin_dir.display());

    for (binary_name, source_path) in sidecars_to_preserve {
      let dest_path = bin_dir.join(&binary_name);

      if dest_path.exists() {
        log::info!(
          "Restoring unstripped sidecar: {} (preserves byte-equivalence)",
          binary_name
        );

        fs::copy(&source_path, &dest_path).with_context(|| {
          format!(
            "Failed to restore unstripped sidecar binary '{}'",
            binary_name
          )
        })?;

        // Ensure the restored binary is executable
        #[cfg(unix)]
        {
          use std::os::unix::fs::PermissionsExt;
          let mut perms = fs::metadata(&dest_path)?.permissions();
          perms.set_mode(0o755);
          fs::set_permissions(&dest_path, perms)?;
        }
      } else {
        log::warn!(
          "Sidecar binary '{}' not found at expected location after sharun processing: {}",
          binary_name,
          dest_path.display()
        );
      }
    }
  }

  fs_utils::remove_dir_all(&app_dir_path.join("usr/"))?;

  let sharun = app_dir_path.join("sharun");

  // Verify sharun binary exists before trying to use it
  if !sharun.exists() {
    return Err(crate::Error::GenericError(format!(
      "sharun binary not found at expected location: {}. This may indicate the lib4bin command did not complete successfully.",
      sharun.display()
    )));
  }

  fs::copy(&sharun, app_dir_path.join("AppRun"))?;

  Command::new(sharun)
    .current_dir(&app_dir_path)
    .arg("-g")
    .output_ok()
    .context("Failed to generate library path for AppDir.")?;

  if let Some(upinfo) = upinfo.as_deref() {
    Command::new(&uruntime_lite)
      .current_dir(&app_dir_path)
      .args([
        "--appimage-addupdinfo",
        &upinfo.replace("$ARCH", tools_arch),
      ])
      .output_ok()
      .context("Failed to add update info.")?;
  }

  // TODO: verbosity - uruntime doesn't expose any settings and doesn't log much
  Command::new(&uruntime)
    .env("ARCH", tools_arch)
    // TODO: check if needed like in our old bundler. May not work on the addupinfo call above.
    // .env("APPIMAGE_EXTRACT_AND_RUN", "1")
    .args([
      "--appimage-mkdwarfs",
      "-f",
      "--set-owner",
      "0",
      "--set-group",
      "0",
      "--no-history",
      "--no-create-timestamp",
      "--compression",
      "zstd:level=22",
      "-S26",
      "-B8",
      "--header",
      &uruntime_lite.to_string_lossy(),
      "-i",
      &app_dir_path.to_string_lossy(),
      "-o",
      &appimage_path.to_string_lossy(),
    ])
    .output_ok()
    .context("Failed to generate AppImage from AppDir.")?;

  {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&appimage_path, fs::Permissions::from_mode(0o770))?;
  }

  if upinfo.is_some() {
    Command::new("zsyncmake")
      .args([
        &appimage_path.to_string_lossy(),
        "-u",
        &appimage_path.to_string_lossy(),
      ])
      .output_ok()
      .context("Failed to create .zsync file.")?;
  }

  fs::remove_dir_all(package_dir)?;
  Ok(vec![appimage_path])
}

// TODO: mirror
fn prepare_tools(
  tools_path: &Path,
  arch: &str,
  squashfs: bool,
) -> crate::Result<(PathBuf, PathBuf, PathBuf)> {
  let fstype = if squashfs { "squashfs" } else { "dwarfs" };
  let uruntime = tools_path.join(format!("uruntime-appimage-{fstype}-{arch}"));
  if !uruntime.exists() {
    let data = download(&format!("https://github.com/VHSgunzo/uruntime/releases/download/v0.4.5/uruntime-appimage-{fstype}-{arch}"))?;
    write_and_make_executable(&uruntime, data)?;
  }

  let uruntime_lite = tools_path.join(format!("uruntime-appimage-{fstype}-lite-{arch}"));
  if !uruntime_lite.exists() {
    let data = download(&format!("https://github.com/VHSgunzo/uruntime/releases/download/v0.4.5/uruntime-appimage-{fstype}-lite-{arch}"))?;
    write_and_make_executable(&uruntime_lite, data)?;
  }

  let sharun_aio = tools_path.join(format!("sharun-{arch}-aio"));
  if !sharun_aio.exists() {
    let data = download(&format!(
      "https://github.com/VHSgunzo/sharun/releases/download/v0.7.4/sharun-{arch}-aio"
    ))?;
    write_and_make_executable(&sharun_aio, data)?;
  }

  Ok((sharun_aio, uruntime, uruntime_lite))
}
