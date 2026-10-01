//! macOS の Keychain にトークンを保管する（D-100）。
//! `security` コマンドは使わない（トークンが引数に出て `ps` から見える）。API で直接読み書きする

use security_framework::base::Error as SecError;
use security_framework::passwords::{
    delete_generic_password, get_generic_password, set_generic_password,
};

use crate::secrets::Secrets;
use crate::{Error, Result};

pub const SERVICE: &str = "spindle-agent";
pub const ACCOUNT: &str = "token";
/// トークンの項目が無いときに、書けるかを確かめる試しの項目のアカウント名
pub const PROBE_ACCOUNT: &str = "probe";

/// `errSecItemNotFound`
const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;
/// `errSecInteractionNotAllowed`（ssh 越しなど、Keychain の解錠を求められないとき）
const ERR_SEC_INTERACTION_NOT_ALLOWED: i32 = -25308;
const INTERACTION_HINT: &str = "ssh 越しでは Keychain を使えません。Mac の「ターミナル」で実行するか、SPINDLE_AGENT_SECRETS=file を付けて実行してください（以後の sync も同じ指定で）";

pub struct KeychainSecrets {
    service: String,
    account: String,
}

impl KeychainSecrets {
    pub fn new(service: &str, account: &str) -> Self {
        Self {
            service: service.to_owned(),
            account: account.to_owned(),
        }
    }

    /// 試験の後始末用。無ければ何もしない
    pub fn delete(&self) -> Result<()> {
        match delete_generic_password(&self.service, &self.account) {
            Ok(()) => Ok(()),
            Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(()),
            Err(e) => Err(keychain_error("削除", &e)),
        }
    }
}

/// エラーにはコードとシステムのメッセージだけを入れる（トークンは入れない）
fn keychain_error(op: &str, e: &SecError) -> Error {
    if e.code() == ERR_SEC_INTERACTION_NOT_ALLOWED {
        return Error::Stop(format!("Keychain の{op}に失敗: {e}。{INTERACTION_HINT}"));
    }
    Error::Stop(format!("Keychain の{op}に失敗: {e}"))
}

impl Secrets for KeychainSecrets {
    fn get(&self) -> Result<Option<String>> {
        match get_generic_password(&self.service, &self.account) {
            Ok(bytes) => String::from_utf8(bytes)
                .map(|s| Some(s.trim().to_owned()))
                .map_err(|_| Error::Stop("Keychain のトークンが UTF-8 ではありません".to_owned())),
            Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(None),
            Err(e) => Err(keychain_error("読み出し", &e)),
        }
    }

    fn set(&self, token: &str) -> Result<()> {
        set_generic_password(&self.service, &self.account, token.as_bytes())
            .map_err(|e| keychain_error("保存", &e))
    }

    /// トークンの項目が在れば、同じ値を書き戻してその項目を更新できるか（ACL など）を確かめる。
    /// 無ければ同じサービスの試しの項目（アカウント `probe`）を書いて消す。値はどこにも出さない
    fn check_writable(&self) -> Result<()> {
        match get_generic_password(&self.service, &self.account) {
            Ok(existing) => set_generic_password(&self.service, &self.account, &existing)
                .map_err(|e| keychain_error("保存の確認", &e)),
            Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => {
                set_generic_password(&self.service, PROBE_ACCOUNT, b"probe")
                    .map_err(|e| keychain_error("保存の確認", &e))?;
                match delete_generic_password(&self.service, PROBE_ACCOUNT) {
                    Ok(()) => Ok(()),
                    Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(()),
                    Err(e) => Err(keychain_error("保存の確認の後始末", &e)),
                }
            }
            Err(e) => Err(keychain_error("保存の確認", &e)),
        }
    }
}
