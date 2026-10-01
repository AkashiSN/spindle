//! pair と冪等な初期化（仕様 ⑥「保存先の所有権」）。
//! 副作用（marker・フォルダの作成・改名）の前後のどこで落ちても、`setup.phase` と
//! nonce を手掛かりに続きから再開できる。`sync` の冒頭も `continue_setup` を通る

use crate::ctx::Ctx;
use crate::exec::COPY_ON_STOP;
use crate::local::Marker;
use crate::music::Music;
use crate::pathkey::{canonical_key, random_id};
use crate::secrets::Secrets;
use crate::server::Server;
use crate::state::{ServerInfo, Setup, SetupPhase};
use crate::{Error, Result};

/// ミュージック.app の管理用フォルダ名
pub const FOLDER_NAME: &str = "spindle";
/// 改名前の一時フォルダ名の接頭辞（`spindle-setup-<nonce>`）
pub const SETUP_PREFIX: &str = "spindle-setup-";

const FOLDER_EXISTS: &str =
    "ミュージック.app に「spindle」フォルダが既にあります。改名してから pair し直してください";

pub const MEDIA_FOLDER_STOP: &str = "ミュージックのメディアフォルダが ~/Music/spindle か、それを含むフォルダ（~/Music など）になっています。ミュージック > 設定 > ファイル の「ミュージックメディアフォルダの場所」を ~/Music/spindle を含まない別のフォルダに変えてから、やり直してください";

/// 「ファイルを［ミュージック］フォルダにコピー」が ON なら止める
pub fn check_copy_setting<M: Music, S: Server>(cx: &Ctx<'_, M, S>) -> Result<()> {
    if cx.music.copy_to_library()? == Some(true) {
        return Err(Error::Stop(COPY_ON_STOP.to_owned()));
    }
    Ok(())
}

/// root がミュージックのメディアフォルダかその中なら止める。削除でファイルまで消える・コピーの前提が崩れるため
pub fn check_root_not_media_folder<M: Music, S: Server>(cx: &Ctx<'_, M, S>) -> Result<()> {
    if cx.local.looks_like_media_folder()? {
        return Err(Error::Stop(MEDIA_FOLDER_STOP.to_owned()));
    }
    Ok(())
}

pub fn pair<M: Music, S: Server>(
    cx: &mut Ctx<'_, M, S>,
    secrets: &dyn Secrets,
    url: &str,
    insecure_http: bool,
    code: &str,
) -> Result<()> {
    check_copy_setting(cx)?;
    check_root_not_media_folder(cx)?;
    // 初回の Automation 許可は、ワンタイムコードを使う前に求める
    cx.music.probe()?;
    let resp = cx.server.pair(code)?;
    if cx.state.setup.is_some() {
        if let Some(s) = &cx.state.server {
            if s.device_uuid != resp.device_uuid {
                return Err(Error::Stop(format!(
                    "この Mac は別の端末（{}）として登録されています。~/Music/spindle とミュージック.app の spindle フォルダを片付けてから pair してください",
                    resp.device_name
                )));
            }
        }
    }
    secrets.set(&resp.token)?;
    cx.state.server = Some(ServerInfo {
        url: url.to_owned(),
        insecure_http,
        device_uuid: resp.device_uuid,
        device_name: resp.device_name,
    });
    if cx.state.setup.is_none() {
        cx.state.setup = Some(Setup {
            phase: SetupPhase::Started,
            nonce: random_id()?,
            folder_pid: None,
        });
    }
    cx.save()?;
    continue_setup(cx)
}

fn set_phase<M: Music, S: Server>(cx: &mut Ctx<'_, M, S>, phase: SetupPhase) -> Result<()> {
    if let Some(s) = cx.state.setup.as_mut() {
        s.phase = phase;
    }
    cx.save()
}

/// `setup.phase` が `Done` でなければ、続きを行う
pub fn continue_setup<M: Music, S: Server>(cx: &mut Ctx<'_, M, S>) -> Result<()> {
    loop {
        let (phase, nonce, folder_pid) = match &cx.state.setup {
            Some(s) => (s.phase, s.nonce.clone(), s.folder_pid.clone()),
            None => return Ok(()),
        };
        let device_uuid = match &cx.state.server {
            Some(s) => s.device_uuid.clone(),
            None => return Ok(()),
        };
        match phase {
            SetupPhase::Started => {
                let mine = Marker {
                    device_uuid,
                    nonce: nonce.clone(),
                };
                match cx.local.read_marker()? {
                    Some(m) if m == mine => {}
                    Some(_) => {
                        return Err(Error::Stop(
                            "~/Music/spindle は別の端末のものです。中身を確かめてから pair し直してください"
                                .to_owned(),
                        ))
                    }
                    None if cx.local.is_empty_or_missing()? => {
                        cx.local.write_marker(&mine)?;
                        cx.fp.hit("pair.marker_written")?;
                    }
                    None => {
                        return Err(Error::Stop(
                            "~/Music/spindle が空ではありません。中身を移してから pair してください"
                                .to_owned(),
                        ))
                    }
                }
                set_phase(cx, SetupPhase::Marker)?;
            }
            SetupPhase::Marker => {
                let recorded = match &folder_pid {
                    Some(pid) => cx.music.folder(pid)?,
                    None => None,
                };
                let pid = match recorded {
                    Some(f) => f.persistent_id,
                    None => {
                        let tmp_name = format!("{SETUP_PREFIX}{nonce}");
                        let mut found = cx.music.folders_named(&tmp_name)?;
                        match found.len() {
                            1 => found.remove(0).persistent_id,
                            0 => {
                                if !cx.music.folders_named(FOLDER_NAME)?.is_empty() {
                                    return Err(Error::Stop(FOLDER_EXISTS.to_owned()));
                                }
                                cx.music.create_folder(&tmp_name)?.persistent_id
                            }
                            _ => {
                                return Err(Error::Stop(format!(
                                    "ミュージック.app に「{tmp_name}」フォルダが複数あります。片付けてから pair し直してください"
                                )))
                            }
                        }
                    }
                };
                if let Some(s) = cx.state.setup.as_mut() {
                    s.folder_pid = Some(pid);
                }
                set_phase(cx, SetupPhase::FolderCreated)?;
            }
            SetupPhase::FolderCreated => {
                let pid = folder_pid.ok_or_else(|| {
                    Error::State("setup.folder_pid が無い（FolderCreated）".to_owned())
                })?;
                let Some(folder) = cx.music.folder(&pid)? else {
                    return Err(Error::Stop(
                        "ミュージック.app の初期化中のフォルダが消されました。pair し直してください"
                            .to_owned(),
                    ));
                };
                if canonical_key(&folder.name) != canonical_key(FOLDER_NAME) {
                    let others = cx
                        .music
                        .folders_named(FOLDER_NAME)?
                        .into_iter()
                        .any(|f| f.persistent_id != pid);
                    if others {
                        return Err(Error::Stop(FOLDER_EXISTS.to_owned()));
                    }
                    cx.music.rename_playlist(&pid, FOLDER_NAME)?;
                }
                set_phase(cx, SetupPhase::FolderRenamed)?;
            }
            SetupPhase::FolderRenamed => set_phase(cx, SetupPhase::Done)?,
            SetupPhase::Done => return Ok(()),
        }
    }
}
