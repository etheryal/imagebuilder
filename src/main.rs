// Copyright (c) 2021 Miguel Peláez
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

use std::path::{Path, PathBuf};
use std::process::{exit, Command};
use std::time::Duration;

use cargo_manifest::Manifest;
use clap::Clap;
use log::{debug, error, info};
use simple_logger::SimpleLogger;
use wait_timeout::ChildExt;

use crate::error::*;
use crate::opts::*;

mod error;
mod opts;

fn main() -> Result<(), BootImageError> {
    SimpleLogger::new().init()?;

    let opts: Opts = Opts::parse();

    match opts.subcmd {
        SubCommands::Run(opts) => {
            let binary_path = opts.binary_path.canonicalize()?;
            let diskimage = create_kernel_diskimage(&binary_path, false, true, opts.out)?
                .0
                .expect("Booteable image not found");
            run_vm(diskimage, opts.run_args, opts.timeout);
        },
        SubCommands::Build(opts) => {
            if let Err(err) = build(opts) {
                error!("{}", err.to_string());
                exit(1);
            }
        },
    };

    Ok(())
}

fn build(opts: BuildOpts) -> Result<(), BootImageError> {
    if !opts.out.exists() {
        if !opts.create_out {
            return Err(BootImageError::OutNotExist);
        } else {
            std::fs::create_dir_all(&opts.out)?;
        }
    }

    info!("Compiling kernel...");

    let mut build_cmd = Command::new(env!("CARGO"));
    build_cmd.args(opts.build_cmd.split(" "));

    if !build_cmd.status()?.success() {
        return Err(BootImageError::BuildFailed);
    };

    let kernel_manifest = locate_cargo_manifest::locate_manifest()?;
    let manifest = Manifest::from_path(&kernel_manifest)?;
    let package = manifest.package.ok_or(BootImageError::KernelManifest)?;

    let target_dir_root = kernel_manifest
        .parent()
        .ok_or(BootImageError::KernelRootNotFound)?
        .join("target");

    info!("Creating disk image");

    let target_dir = target_dir_root.join(&opts.target).join("release");
    let kernel_name = package.name;
    let binary_path = target_dir.join(format!("{}.elf", &kernel_name));

    debug!("Using {:?} as kernel binary", &binary_path);

    let diskimage = create_kernel_diskimage(
        &binary_path.canonicalize()?,
        !opts.disable_bios,
        !opts.disable_uefi,
        opts.out,
    )?;

    if let Some(image) = &diskimage.0 {
        info!(
            "Created booteable bios image {} at {}",
            kernel_name,
            image.display()
        );
    }

    if let Some(image) = &diskimage.1 {
        info!(
            "Created booteable uefi image {} at {}",
            kernel_name,
            image.display()
        );
    }

    Ok(())
}

fn run_vm(diskimage: PathBuf, args: String, timeout: Option<u64>) {
    let mut child = Command::new("qemu-system-x86_64")
        .arg("-drive")
        .arg(format!("format=raw,file={}", diskimage.display()))
        .args(args.split(&[' ', '|'][..]))
        .spawn()
        .expect("Failed to start virtual machine");

    let status_code = if let Some(timeout) = timeout {
        let timeout = Duration::from_secs(timeout);

        match child
            .wait_timeout(timeout)
            .expect("Failed to wait for virtual machine")
        {
            Some(status) => status.code(),
            None => {
                // child hasn't exited yet
                child.kill().unwrap();
                child.wait().unwrap().code()
            },
        }
    } else {
        child.wait().expect("Failed to wait for virtual machine").code()
    };

    exit(
        status_code
            .map(|exit| if exit == 5 { 0 } else { exit })
            .unwrap_or(1),
    );
}

fn create_kernel_diskimage(
    kernel_binary_path: &Path, uefi: bool, _bios: bool, out: PathBuf,
) -> Result<(Option<PathBuf>, Option<PathBuf>), CreateDiskImageError> {
    let bootloader_manifest_path = bootloader_locator::locate_bootloader("bootloader")?;
    let kernel_manifest_path = locate_cargo_manifest::locate_manifest()?;

    let mut build_cmd = Command::new(env!("CARGO"));
    build_cmd.current_dir(
        bootloader_manifest_path
            .parent()
            .ok_or(CreateDiskImageError::RootNotFound)?,
    );
    build_cmd.arg("builder");
    build_cmd.arg("--quiet");
    build_cmd.arg("--kernel-manifest").arg(&kernel_manifest_path);
    build_cmd.arg("--kernel-binary").arg(&kernel_binary_path);
    build_cmd.arg("--target-dir").arg(
        kernel_manifest_path
            .parent()
            .ok_or(CreateDiskImageError::RootNotFound)?
            .join("target"),
    );
    build_cmd
        .arg("--out-dir")
        .arg(kernel_binary_path.parent().unwrap());

    if !uefi {
        build_cmd.arg("--firmware").arg("bios");
    }

    if !build_cmd
        .status()
        .map_err(|_| CreateDiskImageError::BuildFailed)?
        .success()
    {
        return Err(CreateDiskImageError::BuildFailed);
    }
    info!("Created images. Copying to output directory");

    let kernel_binary_name = kernel_binary_path
        .file_name()
        .ok_or(CreateDiskImageError::RootNotFound)?
        .to_str()
        .ok_or(CreateDiskImageError::RootNotFound)?;

    let biosimage = kernel_binary_path
        .parent()
        .ok_or(CreateDiskImageError::RootNotFound)?
        .join(format!("bootimage-bios-{}.img", kernel_binary_name));

    let uefiimage = kernel_binary_path
        .parent()
        .ok_or(CreateDiskImageError::RootNotFound)?
        .join(format!("bootimage-uefi-{}.img", kernel_binary_name));

    let bios = if biosimage.exists() {
        let out = &out.join("bios.img");
        std::fs::rename(&biosimage, out).map_err(CreateDiskImageError::Move)?;
        Some(out.canonicalize().map_err(CreateDiskImageError::FindMoved)?)
    } else {
        None
    };

    let uefi = if uefiimage.exists() {
        let out = &out.join("uefi.img");
        std::fs::rename(&uefiimage, out).map_err(CreateDiskImageError::Move)?;
        Some(out.canonicalize().map_err(CreateDiskImageError::FindMoved)?)
    } else {
        None
    };

    Ok((bios, uefi))
}
