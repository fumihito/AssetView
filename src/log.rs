/// シンプルなファイルロガー。
///
/// ログは `%TEMP%\AssetView_debug.log` に追記される。
/// クラッシュ時には同ファイルに [PANIC] エントリが書かれる。
use std::fmt;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;

static PATH: OnceLock<PathBuf> = OnceLock::new();

/// アプリ起動直後に一度だけ呼ぶ。パニックフックも設定する。
pub fn init() {
    let p = std::env::temp_dir().join(format!("{}_debug.log", crate::APP_NAME));

    // 今回の起動ごとにログをリセット
    let _ = std::fs::write(
        &p,
        format!(
            "=== {} {} start ===\n",
            crate::APP_NAME,
            env!("CARGO_PKG_VERSION")
        ),
    );

    PATH.set(p).ok();

    // バックトレースを有効化（シンボル付きビルドなら関数名も出る）
    std::env::set_var("RUST_BACKTRACE", "1");

    std::panic::set_hook(Box::new(|info| {
        append(format!("[PANIC] {info}"));
        // バックトレースをログへ
        let bt = std::backtrace::Backtrace::capture();
        append(format!("[BACKTRACE]\n{bt:#}"));
    }));
}

/// 1行ログを追記する。どこからでも呼べる。
pub fn append(msg: impl fmt::Display) {
    if let Some(p) = PATH.get() {
        if let Ok(mut f) = OpenOptions::new().append(true).open(p) {
            let _ = writeln!(f, "{msg}");
        }
    }
}

/// ログファイルのパスを返す（起動メッセージ等に使う）。
pub fn path() -> String {
    PATH.get()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(log not initialized)".into())
}
