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

/// `errSecItemNotFound`
const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;

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
}
