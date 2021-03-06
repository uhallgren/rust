// Copyright 2015 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! C-compiler probing and detection.
//!
//! This module will fill out the `cc` and `cxx` maps of `Build` by looking for
//! C and C++ compilers for each target configured. A compiler is found through
//! a number of vectors (in order of precedence)
//!
//! 1. Configuration via `target.$target.cc` in `config.toml`.
//! 2. Configuration via `target.$target.android-ndk` in `config.toml`, if
//!    applicable
//! 3. Special logic to probe on OpenBSD
//! 4. The `CC_$target` environment variable.
//! 5. The `CC` environment variable.
//! 6. "cc"
//!
//! Some of this logic is implemented here, but much of it is farmed out to the
//! `cc` crate itself, so we end up having the same fallbacks as there.
//! Similar logic is then used to find a C++ compiler, just some s/cc/c++/ is
//! used.
//!
//! It is intended that after this module has run no C/C++ compiler will
//! ever be probed for. Instead the compilers found here will be used for
//! everything.

use std::collections::HashSet;
use std::{env, iter};
use std::path::{Path, PathBuf};
use std::process::Command;

use build_helper::output;
use cc;

use Build;
use config::Target;
use cache::Interned;

// The `cc` crate doesn't provide a way to obtain a path to the detected archiver,
// so use some simplified logic here. First we respect the environment variable `AR`, then
// try to infer the archiver path from the C compiler path.
// In the future this logic should be replaced by calling into the `cc` crate.
fn cc2ar(cc: &Path, target: &str) -> Option<PathBuf> {
    if let Some(ar) = env::var_os("AR") {
        Some(PathBuf::from(ar))
    } else if target.contains("msvc") {
        None
    } else if target.contains("musl") {
        Some(PathBuf::from("ar"))
    } else if target.contains("openbsd") {
        Some(PathBuf::from("ar"))
    } else {
        let parent = cc.parent().unwrap();
        let file = cc.file_name().unwrap().to_str().unwrap();
        for suffix in &["gcc", "cc", "clang"] {
            if let Some(idx) = file.rfind(suffix) {
                let mut file = file[..idx].to_owned();
                file.push_str("ar");
                return Some(parent.join(&file));
            }
        }
        Some(parent.join(file))
    }
}

pub fn find(build: &mut Build) {
    // For all targets we're going to need a C compiler for building some shims
    // and such as well as for being a linker for Rust code.
    let targets = build.targets.iter().chain(&build.hosts).cloned().chain(iter::once(build.build))
                               .collect::<HashSet<_>>();
    for target in targets.into_iter() {
        let mut cfg = cc::Build::new();
        cfg.cargo_metadata(false).opt_level(0).warnings(false).debug(false)
           .target(&target).host(&build.build);
        if target.contains("msvc") {
            cfg.static_crt(true);
        }

        let config = build.config.target_config.get(&target);
        if let Some(cc) = config.and_then(|c| c.cc.as_ref()) {
            cfg.compiler(cc);
        } else {
            set_compiler(&mut cfg, Language::C, target, config, build);
        }

        let compiler = cfg.get_compiler();
        let ar = if let ar @ Some(..) = config.and_then(|c| c.ar.clone()) {
            ar
        } else {
            cc2ar(compiler.path(), &target)
        };

        build.verbose(&format!("CC_{} = {:?}", &target, compiler.path()));
        build.cc.insert(target, compiler);
        if let Some(ar) = ar {
            build.verbose(&format!("AR_{} = {:?}", &target, ar));
            build.ar.insert(target, ar);
        }
    }

    // For all host triples we need to find a C++ compiler as well
    let hosts = build.hosts.iter().cloned().chain(iter::once(build.build)).collect::<HashSet<_>>();
    for host in hosts.into_iter() {
        let mut cfg = cc::Build::new();
        cfg.cargo_metadata(false).opt_level(0).warnings(false).debug(false).cpp(true)
           .target(&host).host(&build.build);
        let config = build.config.target_config.get(&host);
        if let Some(cxx) = config.and_then(|c| c.cxx.as_ref()) {
            cfg.compiler(cxx);
        } else {
            set_compiler(&mut cfg, Language::CPlusPlus, host, config, build);
        }
        let compiler = cfg.get_compiler();
        build.verbose(&format!("CXX_{} = {:?}", host, compiler.path()));
        build.cxx.insert(host, compiler);
    }
}

fn set_compiler(cfg: &mut cc::Build,
                compiler: Language,
                target: Interned<String>,
                config: Option<&Target>,
                build: &Build) {
    match &*target {
        // When compiling for android we may have the NDK configured in the
        // config.toml in which case we look there. Otherwise the default
        // compiler already takes into account the triple in question.
        t if t.contains("android") => {
            if let Some(ndk) = config.and_then(|c| c.ndk.as_ref()) {
                let target = target.replace("armv7", "arm");
                let compiler = format!("{}-{}", target, compiler.clang());
                cfg.compiler(ndk.join("bin").join(compiler));
            }
        }

        // The default gcc version from OpenBSD may be too old, try using egcc,
        // which is a gcc version from ports, if this is the case.
        t if t.contains("openbsd") => {
            let c = cfg.get_compiler();
            let gnu_compiler = compiler.gcc();
            if !c.path().ends_with(gnu_compiler) {
                return
            }

            let output = output(c.to_command().arg("--version"));
            let i = match output.find(" 4.") {
                Some(i) => i,
                None => return,
            };
            match output[i + 3..].chars().next().unwrap() {
                '0' ... '6' => {}
                _ => return,
            }
            let alternative = format!("e{}", gnu_compiler);
            if Command::new(&alternative).output().is_ok() {
                cfg.compiler(alternative);
            }
        }

        "mips-unknown-linux-musl" => {
            if cfg.get_compiler().path().to_str() == Some("gcc") {
                cfg.compiler("mips-linux-musl-gcc");
            }
        }
        "mipsel-unknown-linux-musl" => {
            if cfg.get_compiler().path().to_str() == Some("gcc") {
                cfg.compiler("mipsel-linux-musl-gcc");
            }
        }

        t if t.contains("musl") => {
            if let Some(root) = build.musl_root(target) {
                let guess = root.join("bin/musl-gcc");
                if guess.exists() {
                    cfg.compiler(guess);
                }
            }
        }

        _ => {}
    }
}

/// The target programming language for a native compiler.
enum Language {
    /// The compiler is targeting C.
    C,
    /// The compiler is targeting C++.
    CPlusPlus,
}

impl Language {
    /// Obtains the name of a compiler in the GCC collection.
    fn gcc(self) -> &'static str {
        match self {
            Language::C => "gcc",
            Language::CPlusPlus => "g++",
        }
    }

    /// Obtains the name of a compiler in the clang suite.
    fn clang(self) -> &'static str {
        match self {
            Language::C => "clang",
            Language::CPlusPlus => "clang++",
        }
    }
}
