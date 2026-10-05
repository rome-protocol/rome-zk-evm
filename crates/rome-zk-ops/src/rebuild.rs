//! The rebuild: the batch guest for one genesis, built from the public sources in the pinned guest-build image
//! (`deploy/rollup/guest-build`), and what that build reports (the ELF's sha256, the genesis it embedded, the chain id
//! and, when the ZisK proving keys are mounted, the programVK).
//!
//! [`Rebuilder`] is the seam: `vkey register` is generic over it, so its tests run over a fake that answers from a
//! table. [`DockerRebuilder`] is the real one: it builds the image for the ZisK release and the two source tags (the
//! same image and tag `./rollup guest-build` uses, so the layers are shared) and runs it with the genesis mounted
//! read-only. The release is the request's: each release has its own image, with its own toolchain and its own pins
//! (`zisk/<release>.env` in the guest-build directory), and its own proving keys: the image refuses the keys of
//! another release, so each rebuild mounts the directory that was given for its release.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// What a rebuild reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rebuilt {
    /// sha256 of the built ELF, lowercase hex.
    pub elf_sha256: String,
    /// sha256 of the genesis file the guest embedded, lowercase hex.
    pub genesis_sha256: String,
    pub chain_id: u64,
    /// The one backed balance the genesis gives an account, in lamports (the build prints `none` for a genesis with
    /// no balance). Rome checks the chain's vault holds it before registering the key.
    pub genesis_balance: u64,
    /// The wei left over when the balance is not a whole number of lamports (the build adds `remainder_wei=` to its
    /// line then). Zero for a balance that is whole lamports, or no balance.
    pub genesis_balance_remainder_wei: u64,
    /// `None` when the build had no proving keys, so the programVK was not computed.
    pub program_vk: Option<[u8; 32]>,
}

#[allow(async_fn_in_trait)]
pub trait Rebuilder {
    /// Whether a rebuild for the ZisK release `zisk` can start: its proving keys are there. `Err` is the reason, naming
    /// the release. Asked for every release a command will rebuild, before the first rebuild (which takes minutes).
    fn can_rebuild(&self, zisk: &str) -> Result<(), String>;

    /// Builds the guest for `genesis` at `guest_tag`, on the node at `evm_tag`, with the toolchain of the ZisK release
    /// `zisk` (for example `1.3.1-alpha`). `Err` is the reason the build did not finish, as the build itself named it.
    async fn rebuild(
        &self,
        genesis: &Path,
        chain_id: u64,
        guest_tag: &str,
        evm_tag: &str,
        zisk: &str,
    ) -> Result<Rebuilt, String>;
}

/// Runs the guest-build image through a docker command.
#[derive(Debug, Clone)]
pub struct DockerRebuilder {
    /// The docker command and any prefix, for example `["docker"]` or `["sudo", "docker"]`.
    pub docker: Vec<String>,
    /// The directory holding the image's Dockerfile (`deploy/rollup/guest-build`).
    pub context: PathBuf,
    /// One proving key directory per ZisK release (each the one that holds `zisk/vadcop_final` for that release). A
    /// rebuild mounts the directory of its own release: the image refuses the keys of another release
    /// (`ProvingKeyMismatch`), so one directory cannot serve two releases. A release with no directory is not rebuilt.
    pub proving_key_dirs: BTreeMap<String, PathBuf>,
}

/// A source tag goes into a docker build argument and an image name, so it is a plain tag or it is refused.
pub fn tag_is_plain(tag: &str) -> bool {
    !tag.is_empty()
        && tag.len() <= 100
        && tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && !tag.starts_with(['-', '.'])
}

/// The name of the image for one ZisK release and two source tags: the same name `./rollup guest-build` gives it.
pub fn image_name(zisk: &str, evm_tag: &str, guest_tag: &str) -> String {
    format!("rome-zk-guest-build:{zisk}-{evm_tag}-{guest_tag}")
}

/// One value of a release's pin file, `zisk/<release>.env` in the guest-build directory: the first line that starts
/// with `key=`. A release with no pin file is not one the image can be built for.
pub fn pin_value(context: &Path, release: &str, key: &str) -> Result<String, String> {
    let file = context.join("zisk").join(format!("{release}.env"));
    let text = std::fs::read_to_string(&file).map_err(|e| {
        format!(
            "ZiskReleaseUnknown: ZisK {release} has no pin file {} ({e})",
            file.display()
        )
    })?;
    value(&text, key)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("PinFileInvalid: {} has no {key}", file.display()))
}

/// The value after `key=` on the first line that starts with it.
fn value<'a>(out: &'a str, key: &str) -> Option<&'a str> {
    out.lines()
        .find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
        .map(str::trim)
}

fn hex_digest(s: &str) -> Option<String> {
    (s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())).then(|| s.to_ascii_lowercase())
}

/// The balance line the pinned guest build prints: `none`, or `address=0x… wei=… lamports=…` with ` remainder_wei=…`
/// added when the wei is not a whole number of lamports. Returns the lamports and the remainder in wei.
fn parse_balance(v: &str) -> Result<(u64, u64), String> {
    if v == "none" {
        return Ok((0, 0));
    }
    let field = |name: &str| {
        v.split_whitespace()
            .find_map(|t| t.strip_prefix(name).and_then(|r| r.strip_prefix('=')))
    };
    let number = |name: &str, text: &str| {
        text.parse::<u64>().map_err(|_| {
            format!("the build's genesis_balance has {name}='{text}', which is not a whole number that fits 64 bits (line: '{v}')")
        })
    };
    let lamports = field("lamports")
        .ok_or_else(|| format!("the build's genesis_balance line '{v}' has no lamports= field"))?;
    let lamports = number("lamports", lamports)?;
    let remainder = match field("remainder_wei") {
        None => 0,
        Some(r) => number("remainder_wei", r)?,
    };
    Ok((lamports, remainder))
}

/// Parses the lines the guest-build image prints on stdout.
pub fn parse_output(out: &str) -> Result<Rebuilt, String> {
    let elf_sha256 = value(out, "elf_sha256")
        .and_then(hex_digest)
        .ok_or("the build printed no elf_sha256")?;
    let genesis_sha256 = value(out, "genesis_sha256")
        .and_then(hex_digest)
        .ok_or("the build printed no genesis_sha256")?;
    let chain_id = value(out, "chain_id")
        .and_then(|v| v.parse().ok())
        .ok_or("the build printed no chain_id")?;
    let (genesis_balance, genesis_balance_remainder_wei) = match value(out, "genesis_balance") {
        None => return Err("the build printed no genesis_balance line".to_string()),
        Some(v) => parse_balance(v)?,
    };
    let program_vk = match value(out, "program_vk") {
        None => return Err("the build printed no program_vk line".to_string()),
        Some("none") => None,
        Some(v) => {
            let bytes = hex::decode(v.trim_start_matches("0x"))
                .ok()
                .and_then(|b| <[u8; 32]>::try_from(b).ok())
                .ok_or_else(|| format!("the build's program_vk '{v}' is not 32 bytes of hex"))?;
            Some(bytes)
        }
    };
    Ok(Rebuilt {
        elf_sha256,
        genesis_sha256,
        chain_id,
        genesis_balance,
        genesis_balance_remainder_wei,
        program_vk,
    })
}

impl DockerRebuilder {
    /// The proving key directory given for `zisk`, when it is a directory. The refusal names the release.
    fn keys_for(&self, zisk: &str) -> Result<&Path, String> {
        let dir = self.proving_key_dirs.get(zisk).ok_or_else(|| {
            format!(
                "no proving key directory was given for ZisK {zisk}; the programVK is recomputed from the rebuild, and the image refuses the keys of another release"
            )
        })?;
        if dir.is_dir() {
            Ok(dir)
        } else {
            Err(format!(
                "the proving key directory for ZisK {zisk}, {}, is not a directory",
                dir.display()
            ))
        }
    }

    fn docker_cmd(&self) -> Result<Command, String> {
        let (first, rest) = self
            .docker
            .split_first()
            .ok_or("the docker command is empty")?;
        let mut c = Command::new(first);
        c.args(rest);
        Ok(c)
    }

    fn rebuild_blocking(
        &self,
        genesis: &Path,
        chain_id: u64,
        guest_tag: &str,
        evm_tag: &str,
        zisk: &str,
    ) -> Result<Rebuilt, String> {
        if !tag_is_plain(guest_tag) || !tag_is_plain(evm_tag) || !tag_is_plain(zisk) {
            return Err(format!(
                "the source tags and the ZisK release must be plain names (letters, digits, '.', '_', '-'); got guest '{guest_tag}', node '{evm_tag}', ZisK '{zisk}'"
            ));
        }
        // The release's own toolchain tag: the image is labelled with it and refuses to build unless it is the pin file's.
        let toolchain_tag = pin_value(&self.context, zisk, "ZISK_TOOLCHAIN_TAG")?;
        // The release's own proving keys, before anything is built.
        let keys = self.keys_for(zisk)?.to_path_buf();
        let genesis = std::fs::canonicalize(genesis)
            .map_err(|e| format!("cannot resolve {}: {e}", genesis.display()))?;
        let image = image_name(zisk, evm_tag, guest_tag);

        // The image: a cache hit when `./rollup guest-build` or an earlier run built the same release and tags.
        let status = self
            .docker_cmd()?
            .args(["build", "--platform", "linux/amd64", "-t", &image])
            .arg("--build-arg")
            .arg(format!("ZISK_RELEASE={zisk}"))
            .arg("--build-arg")
            .arg(format!("ZISK_TOOLCHAIN_TAG={toolchain_tag}"))
            .arg("--build-arg")
            .arg(format!("ROME_ZK_EVM_TAG={evm_tag}"))
            .arg("--build-arg")
            .arg(format!("ROME_ZK_GUEST_TAG={guest_tag}"))
            .arg(&self.context)
            .stdout(Stdio::null())
            .status()
            .map_err(|e| format!("cannot run docker: {e}"))?;
        if !status.success() {
            return Err(format!("docker build of {image} failed ({status})"));
        }

        // The output directory is ours: created here, removed here.
        let out_dir = std::env::temp_dir().join(format!(
            "rome-zk-ops-vkey-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&out_dir)
            .map_err(|e| format!("cannot create {}: {e}", out_dir.display()))?;
        let result = self.run_image(&image, &genesis, &keys, &out_dir, chain_id);
        let _ = std::fs::remove_dir_all(&out_dir);
        result
    }

    fn run_image(
        &self,
        image: &str,
        genesis: &Path,
        keys: &Path,
        out_dir: &Path,
        chain_id: u64,
    ) -> Result<Rebuilt, String> {
        use std::os::unix::fs::MetadataExt;
        let owner = std::fs::metadata(out_dir).map_err(|e| e.to_string())?;
        let mut cmd = self.docker_cmd()?;
        cmd.args(["run", "--rm", "--platform", "linux/amd64"])
            .arg("--mount")
            .arg(format!(
                "type=bind,source={},target=/in/genesis.json,readonly",
                genesis.display()
            ))
            .arg("--mount")
            .arg(format!(
                "type=bind,source={},target=/out",
                out_dir.display()
            ));
        let keys = std::fs::canonicalize(keys)
            .map_err(|e| format!("cannot resolve {}: {e}", keys.display()))?;
        cmd.arg("--mount").arg(format!(
            "type=bind,source={},target=/keys/provingKey,readonly",
            keys.display()
        ));
        cmd.arg("-e")
            .arg(format!("OUT_UID={}", owner.uid()))
            .arg("-e")
            .arg(format!("OUT_GID={}", owner.gid()))
            .arg(image)
            .arg("--expect-chain-id")
            .arg(chain_id.to_string())
            .arg("--proving-key-dir")
            .arg("/keys/provingKey");
        // The image logs to stderr (the build steps, and the name of a refusal); that goes straight through.
        let out = cmd
            .stderr(Stdio::inherit())
            .output()
            .map_err(|e| format!("cannot run docker: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "the guest build in {image} stopped ({}); its reason is on the lines above",
                out.status
            ));
        }
        parse_output(&String::from_utf8_lossy(&out.stdout))
    }
}

impl Rebuilder for DockerRebuilder {
    fn can_rebuild(&self, zisk: &str) -> Result<(), String> {
        self.keys_for(zisk).map(|_| ())
    }

    async fn rebuild(
        &self,
        genesis: &Path,
        chain_id: u64,
        guest_tag: &str,
        evm_tag: &str,
        zisk: &str,
    ) -> Result<Rebuilt, String> {
        let this = self.clone();
        let genesis = genesis.to_path_buf();
        let tag = guest_tag.to_string();
        let evm_tag = evm_tag.to_string();
        let zisk = zisk.to_string();
        tokio::task::spawn_blocking(move || {
            this.rebuild_blocking(&genesis, chain_id, &tag, &evm_tag, &zisk)
        })
        .await
        .map_err(|e| format!("the rebuild task failed: {e}"))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12";
    const GSHA: &str = "cd34cd34cd34cd34cd34cd34cd34cd34cd34cd34cd34cd34cd34cd34cd34cd34";

    #[test]
    fn parses_what_the_image_prints() {
        let out = format!(
            "guest_elf={SHA}.elf\nelf_sha256={SHA}\ngenesis_sha256={GSHA}\nchain_id=77\ngenesis_balance=none\nprogram_vk=0x{}\n",
            "11".repeat(32)
        );
        let r = parse_output(&out).unwrap();
        assert_eq!(r.elf_sha256, SHA);
        assert_eq!(r.genesis_sha256, GSHA);
        assert_eq!(r.chain_id, 77);
        assert_eq!(r.genesis_balance, 0);
        assert_eq!(r.genesis_balance_remainder_wei, 0);
        assert_eq!(r.program_vk, Some([0x11; 32]));
    }

    /// The line the pinned guest build (v0.2.0) prints for a genesis with one funded account.
    const V020_LINE: &str = "genesis_balance=address=0x1111111111111111111111111111111111111111 wei=1500000000000000000 lamports=1500000000";

    fn with_balance(line: &str) -> String {
        format!("elf_sha256={SHA}\ngenesis_sha256={GSHA}\nchain_id=77\n{line}\nprogram_vk=none\n")
    }

    #[test]
    fn the_exact_v0_2_0_balance_line_gives_its_lamports() {
        let r = parse_output(&with_balance(V020_LINE)).unwrap();
        assert_eq!(r.genesis_balance, 1_500_000_000);
        assert_eq!(r.genesis_balance_remainder_wei, 0);
    }

    #[test]
    fn a_remainder_in_wei_is_reported_not_rounded() {
        let line = "genesis_balance=address=0x1111111111111111111111111111111111111111 wei=1000000001 lamports=1 remainder_wei=1";
        let r = parse_output(&with_balance(line)).unwrap();
        assert_eq!(r.genesis_balance, 1);
        assert_eq!(r.genesis_balance_remainder_wei, 1);
        // An explicit zero remainder is a whole number of lamports.
        let whole = format!("{V020_LINE} remainder_wei=0");
        assert_eq!(
            parse_output(&with_balance(&whole))
                .unwrap()
                .genesis_balance_remainder_wei,
            0
        );
    }

    #[test]
    fn a_balance_line_that_is_not_the_builds_is_refused() {
        // A bare number is not what the build prints.
        for line in [
            "genesis_balance=1500000000",
            "genesis_balance=address=0x11 wei=5",
            "genesis_balance=address=0x11 wei=5 lamports=1.5",
            "genesis_balance=address=0x11 wei=5 lamports=99999999999999999999",
            "genesis_balance=address=0x11 wei=5 lamports=1 remainder_wei=x",
        ] {
            assert!(parse_output(&with_balance(line)).is_err(), "{line}");
        }
    }

    #[test]
    fn a_build_without_keys_has_no_program_vk() {
        let out =
            format!("elf_sha256={SHA}\ngenesis_sha256={GSHA}\nchain_id=77\ngenesis_balance=none\nprogram_vk=none\n");
        assert_eq!(parse_output(&out).unwrap().program_vk, None);
    }

    #[test]
    fn output_missing_a_field_or_with_a_bad_value_is_refused() {
        assert!(parse_output("elf_sha256=zz\n").is_err());
        let no_vk =
            format!("elf_sha256={SHA}\ngenesis_sha256={GSHA}\nchain_id=77\ngenesis_balance=none\n");
        let no_balance =
            format!("elf_sha256={SHA}\ngenesis_sha256={GSHA}\nchain_id=77\nprogram_vk=none\n");
        assert!(parse_output(&no_balance).is_err());
        let bad_balance = format!(
            "elf_sha256={SHA}\ngenesis_sha256={GSHA}\nchain_id=77\ngenesis_balance=address=0x11 wei=5 lamports=1.5\nprogram_vk=none\n"
        );
        assert!(parse_output(&bad_balance).is_err());
        assert!(parse_output(&no_vk).is_err());
        let bad_vk =
            format!("elf_sha256={SHA}\ngenesis_sha256={GSHA}\nchain_id=77\ngenesis_balance=none\nprogram_vk=0x12\n");
        assert!(parse_output(&bad_vk).is_err());
    }

    #[test]
    fn only_plain_tags_reach_docker() {
        assert!(tag_is_plain("v0.2.0"));
        assert!(tag_is_plain("release-1_2"));
        assert!(!tag_is_plain(""));
        assert!(!tag_is_plain("v0.2.0 --privileged"));
        assert!(!tag_is_plain("-v"));
        assert!(!tag_is_plain("a/b"));
        assert!(!tag_is_plain("a;b"));
    }

    // ---- the release in the image ------------------------------------------------------------------------------------

    /// The guest-build directory of this checkout. It is joined piece by piece: the pin files in it are read here, and a
    /// literal path would have to be listed with the files the change scoping treats as read by Rust tests.
    fn guest_build_dir() -> PathBuf {
        ["..", "..", "deploy", "rollup", "guest-build"]
            .iter()
            .fold(PathBuf::from(env!("CARGO_MANIFEST_DIR")), |p, c| p.join(c))
    }

    /// A docker command that records what it was asked and answers like the image: `build` succeeds, `run` prints the lines
    /// the image prints. Returns the script and the log it appends to.
    fn stub_docker(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("rome-zk-ops-rebuild-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("calls.log");
        let script = dir.join("docker");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"$*\" >> {log}\ncase \"$1\" in\n  build) exit 0 ;;\n  run) printf 'elf_sha256={SHA}\\ngenesis_sha256={GSHA}\\nchain_id=77\\ngenesis_balance=none\\nprogram_vk=none\\n'; exit 0 ;;\nesac\nexit 0\n",
                log = log.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let genesis = dir.join("genesis.json");
        std::fs::write(&genesis, "{}").unwrap();
        (dir, log, genesis)
    }

    /// The directory a test gives as the proving keys of one release: one of its own per release.
    fn keys_dir(dir: &Path, release: &str) -> PathBuf {
        dir.join(format!("keys-{release}"))
    }

    fn rebuilder(dir: &Path) -> DockerRebuilder {
        let proving_key_dirs = ["1.2.0-alpha", "1.3.1-alpha"]
            .iter()
            .map(|r| {
                let keys = keys_dir(dir, r);
                std::fs::create_dir_all(&keys).unwrap();
                (r.to_string(), keys)
            })
            .collect();
        DockerRebuilder {
            docker: vec![dir.join("docker").display().to_string()],
            context: guest_build_dir(),
            proving_key_dirs,
        }
    }

    #[test]
    fn each_rebuild_mounts_the_proving_keys_of_its_own_release() {
        let (dir, log, genesis) = stub_docker("keys");
        let rb = rebuilder(&dir);
        for release in ["1.3.1-alpha", "1.2.0-alpha"] {
            rb.rebuild_blocking(&genesis, 77, "v0.2.0", "v0.2.2", release)
                .unwrap();
        }
        let calls = std::fs::read_to_string(&log).unwrap();
        let runs: Vec<&str> = calls.lines().filter(|l| l.starts_with("run ")).collect();
        assert_eq!(runs.len(), 2, "{calls}");
        for (run, release, other) in [
            (runs[0], "1.3.1-alpha", "1.2.0-alpha"),
            (runs[1], "1.2.0-alpha", "1.3.1-alpha"),
        ] {
            let own = std::fs::canonicalize(keys_dir(&dir, release)).unwrap();
            let theirs = std::fs::canonicalize(keys_dir(&dir, other)).unwrap();
            assert!(
                run.contains(&format!(
                    "source={},target=/keys/provingKey,readonly",
                    own.display()
                )),
                "{release} must mount its own keys: {run}"
            );
            assert!(
                !run.contains(&theirs.display().to_string()),
                "{release} must not mount the keys of {other}: {run}"
            );
            assert!(run.contains("--proving-key-dir /keys/provingKey"), "{run}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_release_with_no_proving_key_directory_is_refused_by_name_and_builds_nothing() {
        let (dir, log, genesis) = stub_docker("nokeys");
        let mut rb = rebuilder(&dir);
        rb.proving_key_dirs.remove("1.2.0-alpha");
        let e = rb.can_rebuild("1.2.0-alpha").unwrap_err();
        assert!(
            e.starts_with("no proving key directory") && e.contains("1.2.0-alpha")
                || e.contains("is not a directory") && e.contains("1.2.0-alpha"),
            "{e}"
        );
        assert!(rb.can_rebuild("1.3.1-alpha").is_ok());
        let e = rb
            .rebuild_blocking(&genesis, 77, "v0.2.0", "v0.2.2", "1.2.0-alpha")
            .unwrap_err();
        assert!(
            e.starts_with("no proving key directory") && e.contains("1.2.0-alpha")
                || e.contains("is not a directory") && e.contains("1.2.0-alpha"),
            "{e}"
        );
        assert!(!log.exists(), "docker must not run: {e}");
        // A path that is not a directory is the same refusal.
        rb.proving_key_dirs
            .insert("1.2.0-alpha".into(), dir.join("not-there"));
        let e = rb.can_rebuild("1.2.0-alpha").unwrap_err();
        assert!(
            e.starts_with("no proving key directory") && e.contains("1.2.0-alpha")
                || e.contains("is not a directory") && e.contains("1.2.0-alpha"),
            "{e}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_image_is_named_for_the_release_and_the_two_tags() {
        assert_eq!(
            image_name("1.3.1-alpha", "v0.3.0", "v0.3.0"),
            "rome-zk-guest-build:1.3.1-alpha-v0.3.0-v0.3.0"
        );
        assert_ne!(
            image_name("1.2.0-alpha", "v0.2.2", "v0.2.0"),
            image_name("1.3.1-alpha", "v0.2.2", "v0.2.0"),
            "two releases never share an image"
        );
    }

    #[test]
    fn the_build_is_for_the_release_in_the_request() {
        for (release, toolchain) in [("1.2.0-alpha", "zisk-3.0.0"), ("1.3.1-alpha", "zisk-4.0.0")] {
            let (dir, log, genesis) = stub_docker(release);
            let r = rebuilder(&dir)
                .rebuild_blocking(&genesis, 77, "v0.2.0", "v0.2.2", release)
                .unwrap();
            assert_eq!(r.chain_id, 77);
            let calls = std::fs::read_to_string(&log).unwrap();
            let image = format!("rome-zk-guest-build:{release}-v0.2.2-v0.2.0");
            let build = calls.lines().find(|l| l.starts_with("build ")).unwrap();
            assert!(build.contains(&format!("-t {image} ")), "{build}");
            assert!(
                build.contains(&format!("--build-arg ZISK_RELEASE={release} ")),
                "{build}"
            );
            assert!(
                build.contains(&format!("--build-arg ZISK_TOOLCHAIN_TAG={toolchain} ")),
                "the toolchain tag is the release's pin: {build}"
            );
            let run = calls.lines().find(|l| l.starts_with("run ")).unwrap();
            assert!(
                run.contains(&format!(" {image} ")),
                "the run is in the same image: {run}"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn a_release_with_no_pin_file_or_no_plain_name_builds_nothing() {
        for release in [
            "9.9.9-alpha",
            "../1.3.1-alpha",
            // A name that reaches a pin file that exists: only the name check refuses it.
            "../zisk/1.3.1-alpha",
            "1.3.1-alpha --privileged",
            "",
        ] {
            let (dir, log, genesis) = stub_docker("unknown");
            let e = rebuilder(&dir)
                .rebuild_blocking(&genesis, 77, "v0.2.0", "v0.2.2", release)
                .unwrap_err();
            assert!(
                e.starts_with("ZiskReleaseUnknown") || e.contains("plain names"),
                "{release}: {e}"
            );
            assert!(!log.exists(), "{release}: docker must not run: {e}");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// The one place a release's numbers could drift apart: the pin file (guest-build and the image), the registry's
    /// release table (the scheme byte an entry carries) and Veritas's table (the root a proof is pinned to).
    #[test]
    fn every_pin_file_agrees_with_the_release_tables() {
        use rome_zk_layouts::registry::ZISK_RELEASES;
        let mut seen = 0;
        for r in ZISK_RELEASES {
            let pin = |k: &str| pin_value(&guest_build_dir(), r.name, k).unwrap();
            assert_eq!(pin("ZISK_VERSION"), r.name);
            assert_eq!(pin("ZISK_SCHEME"), r.scheme.to_string(), "{}", r.name);
            let row = veritas::zisk_version(r.scheme)
                .unwrap_or_else(|| panic!("{} has no Veritas row", r.name));
            assert_eq!(row.name, r.name);
            assert_eq!(
                pin("ROOT_C_VADCOP_FINAL"),
                format!("0x{}", hex::encode(row.root_c)),
                "{}: the pin file's root is not the Veritas row's",
                r.name
            );
            seen += 1;
        }
        let files = std::fs::read_dir(guest_build_dir().join("zisk"))
            .unwrap()
            .filter(|f| {
                f.as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|e| e == "env")
            })
            .count();
        assert_eq!(
            files, seen,
            "a pin file with no release in the registry's table, or the other way round"
        );
    }
}
