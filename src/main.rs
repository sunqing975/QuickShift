use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

#[cfg(windows)]
use std::ffi::OsStr;
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
#[cfg(windows)]
use winapi::um::fileapi::{CreateDirectoryW};
#[cfg(windows)]
use std::ptr;


struct FileMoveTask {
    src: PathBuf,
    dst: PathBuf,
}

/// 创建目录，支持 Windows 超长路径（>260 字符）
#[cfg(windows)]
fn create_dir_with_long_path_support(path: &Path) -> Result<(), std::io::Error> {
    let path_str: Vec<u16> = OsStr::new(path)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let mut prefixed: Vec<u16> = Vec::new();

    // 只对绝对路径添加 \\?\
    if path.is_absolute() {
        prefixed.extend(r"\\?\".encode_utf16());
    }
    prefixed.extend(path_str);

    let result = unsafe { CreateDirectoryW(prefixed.as_ptr(), ptr::null_mut()) };

    if result != 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// 非 Windows 平台使用标准 fs::create_dir_all
#[cfg(not(windows))]
fn create_dir_with_long_path_support(path: &Path) -> Result<(), std::io::Error> {
    fs::create_dir_all(path)
}

/// 移动目录，跳过已存在的文件
fn move_directory_concurrent(src_dir: &Path, dest_dir: &Path) -> Result<()> {
    if !src_dir.exists() {
        return Err(anyhow::anyhow!("Source directory does not exist"));
    }
    if !src_dir.is_dir() {
        return Err(anyhow::anyhow!("Source is not a directory"));
    }

    println!("Scanning files in {:?}...", src_dir);
    let mut tasks = Vec::new();

    for entry in walkdir::WalkDir::new(src_dir).follow_links(false) {
        let entry = entry.context("Failed to read directory entry")?;
        let src_path = entry.path();

        let rel_path = src_path.strip_prefix(src_dir)
            .context("Failed to compute relative path")?;
        let dst_path = dest_dir.join(rel_path);

        tasks.push(FileMoveTask {
            src: src_path.to_path_buf(),
            dst: dst_path,
        });
    }

    if tasks.is_empty() {
        fs::create_dir_all(dest_dir)?;
        println!("Source is empty, ensured destination directory exists.");
        return Ok(());
    }

    // 提前保存总数
    let total_count = tasks.len();

    // 过滤：只处理目标不存在的项
    let tasks_to_process: Vec<_> = tasks
        .into_iter()
        .filter(|task| {
            if task.src.is_dir() {
                !task.dst.exists()
            } else if task.src.is_file() {
                !task.dst.exists()
            } else {
                true
            }
        })
        .collect();

    let to_process_count = tasks_to_process.len();
    let skipped_count = total_count - to_process_count;

    if to_process_count == 0 {
        println!("🎉 All {} files already exist at destination. Nothing to move.", total_count);
        return Ok(());
    }

    println!("Processing {} items ({} skipped).", to_process_count, skipped_count);

    // 进度条
    let pb = ProgressBar::new(to_process_count as u64);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta})")
            .unwrap()
            .progress_chars("#>-"),
    );
    let pb = Mutex::new(pb);

    // 设置线程数为 3（SSD → HDD 最佳）
    rayon::ThreadPoolBuilder::new()
        .num_threads(3)
        .build_global()
        .expect("Failed to set Rayon thread pool");

    // 并发处理
    let results: Vec<Result<()>> = tasks_to_process
        .par_iter()
        .map(|task| {
            let result = move_single_item(&task.src, &task.dst)
                .with_context(|| format!("Failed to move {:?} -> {:?}", task.src, task.dst));

            if result.is_ok() {
                let guard = pb.lock().unwrap();
                guard.inc(1);
            }

            result
        })
        .collect();

    // 收集错误（只返回第一个）
    for result in results {
        if let Err(e) = result {
            eprintln!("Error during move: {:#}", e);
            return Err(e);
        }
    }

    pb.lock().unwrap().finish_with_message("done");

    // 尝试删除源目录（仅当为空）
    if let Err(e) = fs::remove_dir(src_dir) {
        if e.kind() != std::io::ErrorKind::NotFound && e.kind() != std::io::ErrorKind::DirectoryNotEmpty {
            eprintln!("Warning: Could not remove source root dir '{}': {}", src_dir.display(), e);
        }
    }

    Ok(())
}

/// 移动单个文件或目录，支持长路径
fn move_single_item(src: &Path, dst: &Path) -> Result<()> {
    if dst.exists() {
        return Ok(()); // ✅ 所有情况都先检查
    }

    if src.is_dir() {
        let _ = create_dir_with_long_path_support(dst); // 忽略 AlreadyExists
        Ok(())
    } else if src.is_file() {
        if let Some(parent) = dst.parent() {
            let _ = create_dir_with_long_path_support(parent);
        }

        if dst.exists() {
            return Ok(()); // 再次确认
        }

        // 尝试移动
        if fs::rename(src, dst).is_err() {
            // 如果 rename 失败（比如跨盘符），尝试 copy + remove
            // 但 copy 前再检查一次
            if !dst.exists() {
                match fs::copy(src, dst) {
                    Ok(_) => {}
                    Err(e) => {
                        if e.kind() == std::io::ErrorKind::AlreadyExists {
                            return Ok(()); // 安全跳过
                        } else {
                            return Err(e).context("Copy failed");
                        }
                    }
                }
            }
            let _ = fs::remove_file(src); // 尝试删除源，失败也无所谓
        }

        Ok(())
    } else {
        Ok(())
    }
}

fn main() -> Result<()> {
    // ⚠️ 修改为你自己的路径
    let src = Path::new(r"D:\dev\code");   // 例如：SSD 上的文件夹
    let dest = Path::new(r"E:\dev");  // 例如：HDD 上的目标

    if !src.exists() {
        eprintln!("Source path does not exist: {:?}", src);
        std::process::exit(1);
    }

    let start = Instant::now();
    println!("🚀 Starting move from {:?} to {:?}", src, dest);

    match move_directory_concurrent(src, dest) {
        Ok(()) => {
            println!("✅ Success! Total time: {:?}", start.elapsed());
        }
        Err(e) => {
            eprintln!("❌ Move failed: {:#}", e);
            std::process::exit(1);
        }
    }

    Ok(())
}
