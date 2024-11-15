use std::{
    str, path::PathBuf,
    thread::{sleep, spawn},
    process::{exit, Command},
    env::{self, current_exe},
    time::{self, Duration, Instant},
    hash::{DefaultHasher, Hash, Hasher},
    os::unix::fs::{symlink, MetadataExt},
    io::{Error, ErrorKind::{NotFound, InvalidData}, Read, Result, Seek, SeekFrom},
    fs::{self, create_dir, create_dir_all, read_to_string, remove_dir, remove_dir_all, remove_file, File},
};

use which::which;
use cfg_if::cfg_if;
use goblin::elf::Elf;
use memfd_exec::{MemFdExecutable, Stdio};
use nix::sys::{wait::waitpid, signal::{Signal, kill}};
use nix::unistd::{access, fork, getcwd, AccessFlags, ForkResult, Pid};
use signal_hook::{consts::{SIGINT, SIGTERM, SIGQUIT, SIGHUP}, iterator::Signals};


const URUNTIME_VERSION: &str = env!("CARGO_PKG_VERSION");


#[derive(Debug)]
struct Runtime {
    size: u64,
    headers_bytes: Vec<u8>,
}

#[derive(Debug)]
struct Image {
    path: PathBuf,
    offset: u64,
    is_squash: bool,
    is_dwar: bool,
}

#[derive(Debug)]
struct Embed {
    #[cfg(feature = "squashfs")]
    squashfuse: Vec<u8>,
    #[cfg(feature = "squashfs")]
    unsquashfs: Vec<u8>,
    #[cfg(feature = "squashfs")]
    mksquashfs: Vec<u8>,
    #[cfg(feature = "dwarfs")]
    tar: Vec<u8>,
    #[cfg(feature = "dwarfs")]
    dwarfs_universal: Vec<u8>,
}

impl Embed {
    fn new() -> Self {
        Embed {
            #[cfg(feature = "squashfs")]
            squashfuse: include_bytes!("../assets/squashfuse-upx").to_vec(),
            #[cfg(feature = "squashfs")]
            unsquashfs: include_bytes!("../assets/unsquashfs-upx").to_vec(),
            #[cfg(feature = "squashfs")]
            mksquashfs: include_bytes!("../assets/mksquashfs-upx").to_vec(),
            #[cfg(feature = "dwarfs")]
            tar: include_bytes!("../assets/tar-upx").to_vec(),
            #[cfg(feature = "dwarfs")]
            dwarfs_universal: include_bytes!("../assets/dwarfs-universal-upx").to_vec(),
        }
    }

    fn embed_exec(&self, exec_name: &str, exec_bytes: &Vec<u8>, exec_args: Vec<String>) {
        MemFdExecutable::new(exec_name, &exec_bytes)
            .args(exec_args)
            .envs(env::vars())
            .exec(Stdio::inherit());
    }

    #[cfg(feature = "squashfs")]
    fn squashfuse(&self, exec_args: Vec<String>) {
        self.embed_exec("squashfuse", &self.squashfuse, exec_args);
    }

    #[cfg(feature = "squashfs")]
    fn unsquashfs(&self, exec_args: Vec<String>) {
        self.embed_exec("unsquashfs", &self.unsquashfs, exec_args);
    }

    #[cfg(feature = "squashfs")]
    fn mksquashfs(&self, exec_args: Vec<String>) {
        self.embed_exec("mksquashfs", &self.mksquashfs, exec_args);
    }

    #[cfg(feature = "dwarfs")]
    fn dwarfs(&self, exec_args: Vec<String>) {
        self.embed_exec("dwarfs", &self.dwarfs_universal, exec_args);
    }

    #[cfg(feature = "dwarfs")]
    fn dwarfsck(&self, exec_args: Vec<String>) {
        self.embed_exec("dwarfsck", &self.dwarfs_universal, exec_args);
    }

    #[cfg(feature = "dwarfs")]
    fn mkdwarfs(&self, exec_args: Vec<String>) {
        self.embed_exec("mkdwarfs", &self.dwarfs_universal, exec_args);
    }

    #[cfg(feature = "dwarfs")]
    fn dwarfsextract(&self, exec_args: Vec<String>) {
        self.embed_exec("dwarfsextract", &self.dwarfs_universal, exec_args);
    }

    #[cfg(feature = "dwarfs")]
    fn dwarfs_universal(&self, exec_args: Vec<String>) {
        self.embed_exec("dwarfs-universal", &self.dwarfs_universal, exec_args);
    }

    #[cfg(feature = "dwarfs")]
    fn tar(&self, exec_args: Vec<String>) {
        self.embed_exec("tar", &self.tar, exec_args);
    }
}

fn check_memfd_noexec() {
    let noexec_path = PathBuf::from("/proc/sys/vm/memfd_noexec");
    if noexec_path.exists() {
        match read_to_string(&noexec_path) {
            Ok(data) => {
                if !data.contains("0") {
                    eprint!("You need to enable memfd_noexec == 0: {:?} == {data}", &noexec_path);
                    exit(1)
                }
            }
            Err(err) => {
                eprintln!("Failed to read memfd_noexec: {err}: {:?}", &noexec_path);
                exit(1)
            }
        }
    }
}

fn get_image(path: &PathBuf, offset: u64) -> Result<Image> {
    let mut file = File::open(path)?;
    let mut buffer = [0; 4];
    file.seek(SeekFrom::Start(offset))?;
    let bytes_read = file.read(&mut buffer)?;
    let mut result = Image {
        path: path.to_path_buf(),
        offset,
        is_dwar: false,
        is_squash: false
    };
    if bytes_read == 4 {
        let read_str = String::from_utf8_lossy(&buffer);
        if read_str.contains("DWAR") {
            result.is_dwar = true
        } else if read_str.contains("hsqs") {
            result.is_squash = true
        }
    }
    if !result.is_squash && !result.is_dwar {
        return Err(Error::new(NotFound, "SquashFS or DwarFS image not found!"))
    }
    Ok(result)
}

fn get_env_var(env_var: &str) -> String {
    let mut ret = "".to_string();
    if let Ok(res) = env::var(env_var) { ret = res };
    return ret
}

fn add_to_path(path: &PathBuf) {
    let old_path = get_env_var("PATH");
    if old_path.is_empty() {
        env::set_var("PATH", path)
    } else {
        let new_path = path.to_str().unwrap();
        if !old_path.contains(new_path) {
            env::set_var("PATH", format!("{new_path}:{old_path}"))
        }
    }
}

fn check_fuse() {
    let mut is_fusermount = true;
    let tmp_path_dir = &PathBuf::from("/tmp/.path");
    if tmp_path_dir.is_dir() {
        add_to_path(tmp_path_dir)
    }
    let fusermount_prog = &get_env_var("FUSERMOUNT_PROG");
    if PathBuf::from(fusermount_prog).is_file() {
        if !tmp_path_dir.is_dir() {
            if let Err(err) = create_dir_all(tmp_path_dir) {
                eprintln!("Failed to create fusermount PATH dir: {err}: {:?}", tmp_path_dir);
                exit(1)
            }
            add_to_path(tmp_path_dir);
        }
        let fsmntlink_path = tmp_path_dir.join(basename(&fusermount_prog));
        let _ = remove_file(&fsmntlink_path);
        if let Err(err) = symlink(fusermount_prog, &fsmntlink_path) {
            eprintln!("Failed to create fusermount symlink: {err}: {:?}", fsmntlink_path);
            exit(1)
        }
    } else {
        for fusermount in vec!["fusermount", "fusermount3"] {
            let fallback: &str;
            if fusermount.ends_with("3") {
                fallback = "fusermount"
            } else {
                fallback = "fusermount3"
            }
            if which(fusermount).is_err() {
                if let Ok(fusermount_path) = which(fallback) {
                    if !tmp_path_dir.is_dir() {
                        if let Err(err) = create_dir_all(tmp_path_dir) {
                            eprintln!("Failed to create fusermount fallback dir: {err}: {:?}", tmp_path_dir);
                            break
                        }
                    }
                    let fsmntlink_path = tmp_path_dir.join(fusermount);
                    let _ = remove_file(&fsmntlink_path);
                    if let Err(err) = symlink(fusermount_path, &fsmntlink_path) {
                        eprintln!("Failed to create fusermount fallback symlink: {err}: {:?}", tmp_path_dir);
                        break
                    }
                    add_to_path(tmp_path_dir);
                    break
                } else {
                    is_fusermount = false
                }
            }
        }
    }
    if access("/dev/fuse", AccessFlags::R_OK).is_err() ||
       access("/dev/fuse", AccessFlags::W_OK).is_err() || !is_fusermount {
        eprintln!("{}: failed to utilize FUSE during startup", std::env::args().next().unwrap());
        cfg_if!{
            if #[cfg(feature = "appimage")] {
                let arg_pfx = "appimage";
                let self_name = "AppImage";
            } else {
                let arg_pfx = "runtime";
                let self_name = "RunImage";
            }
        }
        eprintln!(
"Cannot mount {self_name}, please check your FUSE setup.

You might still be able to extract the contents of this {self_name}
if you run it with the --{arg_pfx}-extract option
See https://github.com/AppImage/AppImageKit/wiki/FUSE
for more information
");
        exit(1)
    }
}

fn get_runtime(path: &PathBuf) -> Result<Runtime> {
    let mut file = File::open(path)?;
    let mut elf_header_raw = [0; 64];
    file.read_exact(&mut elf_header_raw)?;
    let section_table_offset = u64::from_le_bytes(elf_header_raw[40..48].try_into().unwrap()); // e_shoff
    let section_count = u16::from_le_bytes(elf_header_raw[60..62].try_into().unwrap()); // e_shnum
    let section_table_size = section_count as u64 * 64;
    let required_bytes = section_table_offset + section_table_size;
    let mut headers_bytes = vec![0; required_bytes as usize];
    file.seek(SeekFrom::Start(0))?;
    file.read_exact(&mut headers_bytes)?;
    let elf = Elf::parse(&headers_bytes)
        .map_err(|e| Error::new(InvalidData, e))?;
    let section_table_end =
        elf.header.e_shoff + (elf.header.e_shentsize as u64 * elf.header.e_shnum as u64);
    let last_section_end = elf
        .section_headers
        .last()
        .map(|section| section.sh_offset + section.sh_size)
        .unwrap_or(0);
    Ok(Runtime {
        headers_bytes: headers_bytes.clone(),
        size: section_table_end.max(last_section_end),
    })
}

fn random_string(length: usize) -> String {
    const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = time::SystemTime::now().duration_since(time::UNIX_EPOCH).unwrap().as_secs();
    let mut result = String::with_capacity(length);
    for _ in 0..length {
        rng = rng.wrapping_mul(48271).wrapping_rem(0x7FFFFFFF);
        let idx = (rng % CHARSET.len() as u64) as usize;
        result.push(CHARSET[idx] as char);
    }
    result
}

fn basename(path: &str) -> String {
    let pieces: Vec<&str> = path.rsplit('/').collect();
    pieces.get(0).unwrap().to_string()
}

fn is_mount_point(path: &PathBuf) -> Result<bool> {
    let metadata = fs::metadata(path)?;
    let device_id = metadata.dev();
    match path.parent() {
        Some(parent) => {
            let parent_metadata = fs::metadata(parent)?;
            Ok(device_id != parent_metadata.dev())
        }
        None => Ok(false)
    }
}

fn wait_mount(path: &PathBuf, timeout: Duration) -> bool {
    let start_time = Instant::now();
    while !path.exists() || !is_mount_point(path).unwrap_or(false) {
        if start_time.elapsed() >= timeout {
            eprintln!("Timeout reached while waiting for mount: {:?}", path);
            return false
        }
        sleep(Duration::from_millis(2))
    }
    true
}

fn remove_mnt(mount_dirs: Vec<&PathBuf> ) {
    for dir in mount_dirs {
        let _ = remove_dir(dir);
    }
}

fn mount_image(embed: &Embed, image: &Image, mount_dir: &PathBuf) {
    check_fuse();
    if let Err(err) = create_dir_all(&mount_dir) {
        eprintln!("Failed to create mount dir: {err}: {:?}", mount_dir);
        exit(1)
    }

    let mount_dir = mount_dir.to_str().unwrap().to_string();
    let image_path = image.path.to_str().unwrap().to_string();
    if image.is_dwar {
        #[cfg(feature = "dwarfs")]
        {
            let num_threads = num_cpus::get();
            embed.dwarfs(vec!["-f".into(),
                "-o".into(), "ro,nodev,noatime".into(),
                "-o".into(), format!("offset={}", image.offset),
                "-o".into(), format!("workers={num_threads}"),
                "-o".into(), format!("max_threads={num_threads}"),
                "-o".into(), "debuglevel=error".into(),
                image_path, mount_dir
            ])
        }
    } else {
        #[cfg(feature = "squashfs")]
        {
            let uid = unsafe { nix::libc::getuid() };
            let gid = unsafe { nix::libc::getgid() };
            embed.squashfuse(vec!["-f".into(),
                "-o".into(), "ro,nodev,noatime".into(),
                "-o".into(), format!("uid={},gid={}", uid, gid),
                "-o".into(), format!("offset={}", image.offset),
                image_path, mount_dir
            ])
        }
    }
}

fn extract_image(embed: &Embed, image: &Image, mut extract_dir: PathBuf, is_extract_run: bool, pattern: Option<&String>) {
    if is_extract_run && extract_dir.exists() { return }
    cfg_if!{
        if #[cfg(feature = "appimage")] {
            let applink_dir = extract_dir.join("squashfs-root");
            if !is_extract_run {
                extract_dir = extract_dir.join("AppDir");
            }
        } else {
            if !is_extract_run {
                extract_dir = extract_dir.join("RunDir");
            }
        }
    }
    let extract_dir = extract_dir.to_str().unwrap().to_string();
    if let Err(err) = create_dir_all(&extract_dir) {
        eprintln!("Failed to create extract dir: {err}: {extract_dir}");
        exit(1)
    }
        #[cfg(feature = "appimage")]
        {
            if !is_extract_run {
                let _ = remove_file(&applink_dir);
                if let Err(err) = symlink(&extract_dir, &applink_dir) {
                    eprintln!("Failed to create squashfs-root symlink to extract dir: {err}");
                    exit(1)
                }
            }
        }
    let image_path = image.path.to_str().unwrap().to_string();
    if image.is_dwar {
        #[cfg(feature = "dwarfs")]
        {
            let mut exec_args = vec![
                "--input".into(), image_path,
                "--log-level=error".into(),
                format!("--image-offset={}", image.offset),
                format!("--num-workers={}", num_cpus::get())
            ];
            if pattern.is_some() {
                let tar_args = vec![
                    "-C".into(), extract_dir, "-xvf-".into(),
                    "--wildcards".into(), pattern.unwrap().into()
                ];
                exec_args.push("--format=ustar".into());
                if let Ok(dwarfsextract) = MemFdExecutable::new("dwarfsextract", &embed.dwarfs_universal)
                    .args(exec_args)
                    .stdout(Stdio::piped())
                    .spawn() {
                        if let Ok(mut tar) = MemFdExecutable::new("tar", &embed.tar)
                            .args(tar_args)
                            .stdin(Stdio::piped())
                            .spawn() {
                            let mut dwarfsextract_stdout = dwarfsextract.stdout.unwrap();
                            let mut tar_stdin = tar.stdin.take().unwrap();
                            let _ = std::io::copy(&mut dwarfsextract_stdout, &mut tar_stdin);
                            exit(tar.wait().unwrap().code().unwrap())
                        } else {
                            eprintln!("Failed to run tar!");
                            exit(1)
                        }
                } else {
                    eprintln!("Failed to run dwarfsextract!");
                    exit(1)
                }
            } else {
                exec_args.append(&mut vec![
                    "--output".into(), extract_dir,
                    "--stdout-progress".into()
                ]);
                embed.dwarfsextract(exec_args)
            }
        }
    } else {
        #[cfg(feature = "squashfs")]
        {
            let mut exec_args = vec!["-f".into(),
                "-d".into(), extract_dir,
                "-o".into(), image.offset.to_string(),
                image_path
            ];
            if pattern.is_some() {
                exec_args.push(pattern.unwrap().into());
            }
            embed.unsquashfs(exec_args)
        }
    }
}

fn try_set_portable(kind: &str, dir: &PathBuf) {
    if dir.exists() {
        match kind {
            "home" => {
                println!("Setting $HOME to {:?}", dir);
                env::set_var("HOME", dir)
            }
            "config" => {
                println!("Setting $XDG_CONFIG_HOME to {:?}", dir);
                env::set_var("XDG_CONFIG_HOME", dir)
            }
            _ => {}
        }
    }
}

fn signals_handler(pid: Pid) {
    let mut signals = Signals::new(&[SIGINT, SIGTERM, SIGQUIT]).unwrap();
    let _ = signals.handle();
    for signal in signals.forever() {
        match signal {
            SIGINT | SIGTERM | SIGQUIT | SIGHUP => {
                let _ = kill(pid, Signal::SIGTERM);
                break
            }
            _ => {}
        }
    }
}

fn get_section_data(headers_bytes: Vec<u8>, section_name: &str) -> Result<String> {
    let elf = Elf::parse(&headers_bytes)
        .map_err(|e| Error::new(InvalidData, e))?;
    for section in elf.section_headers.iter() {
        if let Some(name) = elf.shdr_strtab.get_at(section.sh_name) {
            if name == section_name {
                let section_data = &headers_bytes[section.sh_offset as usize..(section.sh_offset + section.sh_size) as usize];
                if let Ok(data_str) = str::from_utf8(section_data) {
                    return Ok(data_str.trim().into());
                } else {
                    return Err(Error::new(InvalidData,
                        format!("Section data is not valid UTF-8: {section_name}")
                    ));
                }
            }
        }
    }
    Err(Error::new(NotFound, format!("Section not found: {section_name}")))
}

fn hash_string(data: &str) -> String {
    let mut hasher = DefaultHasher::new();
    data.hash(&mut hasher);
    hasher.finish().to_string()
}

fn print_usage(portable_home: &PathBuf, portable_config: &PathBuf) {
    cfg_if!{
        if #[cfg(feature = "appimage")] {
            let arg_pfx = "appimage";
            let self_name = "AppImage";
        } else {
            let arg_pfx = "runtime";
            let self_name = "RunImage";
        }
    }
    println!("{} v{URUNTIME_VERSION}

   Runtime options:
    --{arg_pfx}-extract [PATTERN]          Extract content from embedded filesystem image
                                             If pattern is passed, only extract matching files
     --{arg_pfx}-extract-and-run [ARGS]    Run the {self_name} afer extraction without using FUSE
     --{arg_pfx}-offset                    Print byte offset to start of embedded filesystem image
     --{arg_pfx}-portable-home             Create a portable home folder to use as $HOME
     --{arg_pfx}-portable-config           Create a portable config folder to use as $XDG_CONFIG_HOME
     --{arg_pfx}-help                      Print this help
     --{arg_pfx}-version                   Print version of Runtime
     --{arg_pfx}-signature                 Print digital signature embedded in {self_name}
     --{arg_pfx}-updateinfo[rmation]       Print update info embedded in {self_name}
     --{arg_pfx}-mount                     Mount embedded filesystem image and print
                                             mount point and wait for kill with Ctrl-C",
    env!("CARGO_PKG_DESCRIPTION"));

    println!("\n    Embedded tools options:");
    #[cfg(feature = "squashfs")]
    println!("      --{arg_pfx}-squashfuse    [ARGS]       Launch squashfuse");
    #[cfg(feature = "squashfs")]
    println!("      --{arg_pfx}-unsquashfs    [ARGS]       Launch unsquashfs");
    #[cfg(feature = "squashfs")]
    println!("      --{arg_pfx}-mksquashfs    [ARGS]       Launch mksquashfs");
    #[cfg(feature = "dwarfs")]
    println!("      --{arg_pfx}-tar           [ARGS]       Launch tar");
    #[cfg(feature = "dwarfs")]
    println!("      --{arg_pfx}-dwarfs        [ARGS]       Launch dwarfs");
    #[cfg(feature = "dwarfs")]
    println!("      --{arg_pfx}-dwarfsck      [ARGS]       Launch dwarfsck");
    #[cfg(feature = "dwarfs")]
    println!("      --{arg_pfx}-mkdwarfs      [ARGS]       Launch mkdwarfs");
    #[cfg(feature = "dwarfs")]
    println!("      --{arg_pfx}-dwarfsextract [ARGS]       Launch dwarfsextract");
    println!("
      Also you can create a hardlink, symlink or rename the runtime with
      the name of the built-in utility to use it directly.");

    println!("\n    Portable home and config:

      If you would like the application contained inside this {self_name} to store its
      data alongside this {self_name} rather than in your home directory, then you can
      place a directory named

      for portable-home:
      {:?}

      for portable-config:
      {:?}

      Or you can invoke this {self_name} with the --{arg_pfx}-portable-home or
      --{arg_pfx}-portable-config option, which will create this directory for you.
      As long as the directory exists and is neither moved nor renamed, the
      application contained inside this {self_name} to store its data in this
      directory rather than in your home directory", portable_home, portable_config);

    println!("\n    Environment variables:

      {}_EXTRACT_AND_RUN=1      Run the {self_name} afer extraction without using FUSE
      NO_CLEANUP=1                   Do not clear the unpacking directory after closing when
                                       using extract and run option for reuse extracted data
      TMPDIR=/path                   Specifies a custom path for mounting or extracting the image
      FUSERMOUNT_PROG=/path          Specifies a custom path for fusermount", arg_pfx.to_uppercase());
      #[cfg(feature = "appimage")]
      println!("      TARGET_APPIMAGE=/path          Operate on a target {self_name} rather than this file itself");
}

fn main() {
    check_memfd_noexec();
    let embed = Embed::new();

    #[allow(unused_mut)]
    let mut self_exe = &current_exe().unwrap();
    cfg_if!{
        if #[cfg(feature = "appimage")] {
            let target_appimage = PathBuf::from(get_env_var("TARGET_APPIMAGE"));
            if target_appimage.is_file() {
                self_exe = &target_appimage;
            }
        }
    }

    let self_exe_dir = self_exe.parent().unwrap();
    let self_exe_name = self_exe.file_name().unwrap().to_str().unwrap();

    let runtime = get_runtime(self_exe).unwrap_or_else(|err|{
        eprintln!("Failed to get runtime: {err}");
        exit(1)
    });
    let runtime_size = runtime.size;

    let mut exec_args: Vec<String> = env::args().collect();
    let arg0 = &exec_args.remove(0);

    match basename(arg0).as_str() {
        #[cfg(feature = "squashfs")]
        "squashfuse"       => { embed.squashfuse(exec_args); return }
        #[cfg(feature = "squashfs")]
        "unsquashfs"       => { embed.unsquashfs(exec_args); return }
        #[cfg(feature = "squashfs")]
        "mksquashfs"       => { embed.mksquashfs(exec_args); return }
        #[cfg(feature = "dwarfs")]
        "tar" => { embed.tar(exec_args); return }
        #[cfg(feature = "dwarfs")]
        "dwarfs"           => { embed.dwarfs(exec_args); return }
        #[cfg(feature = "dwarfs")]
        "dwarfsck"         => { embed.dwarfsck(exec_args); return }
        #[cfg(feature = "dwarfs")]
        "mkdwarfs"         => { embed.mkdwarfs(exec_args); return }
        #[cfg(feature = "dwarfs")]
        "dwarfsextract"    => { embed.dwarfsextract(exec_args); return }
        #[cfg(feature = "dwarfs")]
        "dwarfs-universal" => { embed.dwarfs_universal(exec_args); return }
        _ => {}
    }

    let arg1 = if !exec_args.is_empty() {
        exec_args[0].to_string()
    } else {"".into()};

    let mut is_noclenup = false;
    let mut is_mount_only = false;
    let mut is_extract_run = false;

    let mut tmp_dir = PathBuf::from(get_env_var("TMPDIR"));
    if !tmp_dir.exists() { tmp_dir = env::temp_dir() }

    cfg_if!{
        if #[cfg(feature = "appimage")] {
            let arg_pfx = "appimage";
            if get_env_var("APPIMAGE_EXTRACT_AND_RUN") == "1" {
                is_extract_run = true
            }
        } else {
            let arg_pfx = "runtime";
            if get_env_var("RUNTIME_EXTRACT_AND_RUN") == "1" {
                is_extract_run = true
            }
        }
    }

    if is_extract_run {
        is_noclenup = get_env_var("NO_CLEANUP") == "1"
    }

    let portable_home = &self_exe_dir.join(format!("{self_exe_name}.home"));
    let portable_config = &self_exe_dir.join(format!("{self_exe_name}.config"));

    if !arg1.is_empty() {
        match arg1 {
            arg if arg == format!("--{arg_pfx}-version") => {
                println!("v{URUNTIME_VERSION}");
                return
            }
            arg if arg == format!("--{arg_pfx}-help") => {
                print_usage(portable_home, portable_config);
                return
            }
            arg if arg == format!("--{arg_pfx}-portable-home") => {
                if let Err(err) = create_dir(portable_home) {
                    eprintln!("Failed to create portable home directory: {:?}: {err}", portable_home)
                }
                println!("Portable home directory created: {:?}", portable_home);
                return
            }
            arg if arg == format!("--{arg_pfx}-portable-config") => {
                if let Err(err) = create_dir(portable_config) {
                    eprintln!("Failed to create portable config directory: {:?}: {err}", portable_config)
                }
                println!("Portable config directory created: {:?}", portable_config);
                return
            }
            arg if arg == format!("--{arg_pfx}-offset") => {
                println!("{runtime_size}");
                return
            }
            arg if arg == format!("--{arg_pfx}-updateinfo") ||
                           arg == format!("--{arg_pfx}-updateinformation") => {
                let updateinfo = get_section_data(runtime.headers_bytes,".upd_info")
                    .unwrap_or_else(|err|{
                        eprintln!("Failed to get update info: {err}");
                        exit(1)
                });
                println!("{updateinfo}");
                return
            }
            arg if arg == format!("--{arg_pfx}-signature") => {
                let signature = get_section_data(runtime.headers_bytes,".sha256_sig")
                    .unwrap_or_else(|err|{
                        eprintln!("Failed to get signature info: {err}");
                        exit(1)
                });
                println!("{signature}");
                return
            }
            #[cfg(feature = "squashfs")]
            arg if arg == format!("--{arg_pfx}-squashfuse") => {
                embed.squashfuse(exec_args[1..].to_vec());
                return
            }
            #[cfg(feature = "squashfs")]
            arg if arg == format!("--{arg_pfx}-unsquashfs") => {
                embed.unsquashfs(exec_args[1..].to_vec());
                return
            }
            #[cfg(feature = "squashfs")]
            arg if arg == format!("--{arg_pfx}-mksquashfs") => {
                embed.mksquashfs(exec_args[1..].to_vec());
                return
            }
            #[cfg(feature = "dwarfs")]
            arg if arg == format!("--{arg_pfx}-tar") => {
                embed.tar(exec_args[1..].to_vec());
                return
            }
            #[cfg(feature = "dwarfs")]
            arg if arg == format!("--{arg_pfx}-dwarfs") => {
                embed.dwarfs(exec_args[1..].to_vec());
                return
            }
            #[cfg(feature = "dwarfs")]
            arg if arg == format!("--{arg_pfx}-dwarfsck") => {
                embed.dwarfsck(exec_args[1..].to_vec());
                return
            }
            #[cfg(feature = "dwarfs")]
            arg if arg == format!("--{arg_pfx}-mkdwarfs") => {
                embed.mkdwarfs(exec_args[1..].to_vec());
                return
            }
            #[cfg(feature = "dwarfs")]
            arg if arg == format!("--{arg_pfx}-dwarfsextract") => {
                embed.dwarfsextract(exec_args[1..].to_vec());
                return
            }
            ref arg if arg == &format!("--{arg_pfx}-extract-and-run") => {
                exec_args.remove(0);
                is_extract_run = true
            }
            _ => {}
        }
    }
    drop(runtime);

    let image = get_image(&self_exe, runtime_size).unwrap_or_else(|err|{
        eprintln!("Failed to get image: {err}");
        exit(1)
    });

    cfg_if!{
        if #[cfg(feature = "appimage")] {
            let mnt_name: String;
            if is_extract_run {
                mnt_name = format!("appimage_extracted_{}", hash_string(&self_exe.to_string_lossy()))
            } else {
                mnt_name = format!(".mount_{}", random_string(8))
            }
            let mount_dir = tmp_dir.join(mnt_name);
            let mount_dirs = vec![&mount_dir];
        } else {
            let uid = unsafe { nix::libc::getuid() };
            let ruid_dir = tmp_dir.join(format!(".r{uid}"));
            let mnt_dir = ruid_dir.join("mnt");
            let mnt_name: String;
            if is_extract_run {
                mnt_name = hash_string(&self_exe.to_string_lossy())
            } else {
                mnt_name = random_string(8)
            }
            let mount_dir = mnt_dir.join(mnt_name);
            let mount_dirs = vec![&mount_dir, &mnt_dir, &ruid_dir];
        }
    }

    if !arg1.is_empty() {
        match arg1 {
            arg if arg == format!("--{arg_pfx}-extract") => {
                extract_image(&embed, &image, PathBuf::from("."),
                    is_extract_run, exec_args.get(1));
                return
            }
            arg if arg == format!("--{arg_pfx}-mount") => {
                println!("{}", mount_dir.display());
                is_mount_only = true
            }
            _ => {}
        }
    }

    match unsafe { fork() } {
        Ok(ForkResult::Parent { child: child_pid }) => {
            if is_extract_run {
                if let Err(err) = waitpid(child_pid, None) {
                    eprintln!("Failed to extract image: {err}");
                    exit(1)
                };
            } else {
                if !wait_mount(&mount_dir, Duration::from_millis(1000)) {
                    remove_mnt(mount_dirs);
                    exit(1)
                }
            }

            let mut exit_code = 143;
            if !is_mount_only {
                cfg_if!{
                    if #[cfg(feature = "appimage")] {
                        let run = mount_dir.join("AppRun");
                        if !run.is_file() {
                            eprintln!("AppRun not found: {:?}", run);
                            remove_mnt(mount_dirs);
                            exit(1)
                        };
                        env::set_var("ARGV0", arg0);
                        env::set_var("APPDIR", &mount_dir);
                        env::set_var("APPIMAGE", self_exe);
                        env::set_var("APPOFFSET", format!("{runtime_size}"));
                    } else {
                        let run = mount_dir.join("static").join("bash");
                        if !run.is_file() {
                            eprintln!("Static bash not found: {:?}", run);
                            remove_mnt(mount_dirs);
                            exit(1)
                        };
                        exec_args.insert(0, format!("{}/Run.sh", mount_dir.display()));
                        env::set_var("ARG0", arg0);
                        env::set_var("RUNDIR", &mount_dir);
                        env::set_var("RUNIMAGE", self_exe);
                        env::set_var("RUNOFFSET", format!("{runtime_size}"));
                    }
                }
                env::set_var("OWD", getcwd().unwrap());

                try_set_portable("home",portable_home);
                try_set_portable("config",portable_config);

                let mut cmd = Command::new(run).args(&exec_args).spawn().unwrap();
                let pid = Pid::from_raw(cmd.id() as i32);

                spawn(move || { signals_handler(pid) });

                if let Ok(status) = cmd.wait() {
                    if let Some(code) = status.code() {
                        exit_code = code
                    }
                }

                if !is_extract_run {
                    let _ = kill(child_pid, Signal::SIGTERM);
                }
            } else {
                signals_handler(child_pid)
            }

            if is_extract_run {
                if !is_noclenup {
                    let _ = remove_dir_all(&mount_dir);
                }
            } else {
                let _ = waitpid(child_pid, None);
            }

            remove_mnt(mount_dirs);

            exit(exit_code)
        }
        Ok(ForkResult::Child) => {
            if is_extract_run {
                extract_image(&embed, &image, mount_dir, is_extract_run, None)
            } else {
                mount_image(&embed, &image, &mount_dir)
            }
        }
        Err(err) => {
            eprintln!("Fork error: {err}");
            exit(1)
        }
    }
}
