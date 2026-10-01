//! 試験の中断点。エンジンは要所（state の保存の直後、ミュージック.app の操作の前後、ファイルの配置の直後）で
//! `hit(名前)` を呼ぶ。`armed()` で作ったものだけが、`arm(名前, n)` した名前の n 回目で `Error::Crash` を返す。
//! 本番は `none()`（何も記録しない）。エンジンは単一スレッドなので `Rc<RefCell<…>>` で共有する

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::{Error, Result};

#[derive(Default)]
struct Inner {
    /// 名前 → 残り回数（1 なら次の hit で発火）
    armed: HashMap<String, u32>,
    seen: Vec<String>,
}

#[derive(Clone, Default)]
pub struct Failpoints(Option<Rc<RefCell<Inner>>>);

impl Failpoints {
    pub fn none() -> Self {
        Self(None)
    }

    pub fn armed() -> Self {
        Self(Some(Rc::new(RefCell::new(Inner::default()))))
    }

    /// `name` の `nth` 回目（1 始まり）の hit で 1 回だけ発火させる
    pub fn arm(&self, name: &str, nth: u32) {
        if let Some(i) = &self.0 {
            i.borrow_mut().armed.insert(name.to_owned(), nth.max(1));
        }
    }

    pub fn hit(&self, name: &str) -> Result<()> {
        let Some(i) = &self.0 else {
            return Ok(());
        };
        let mut i = i.borrow_mut();
        i.seen.push(name.to_owned());
        let fire = match i.armed.get_mut(name) {
            Some(left) if *left <= 1 => true,
            Some(left) => {
                *left -= 1;
                false
            }
            None => false,
        };
        if fire {
            i.armed.remove(name);
            return Err(Error::Crash(name.to_owned()));
        }
        Ok(())
    }

    /// これまでに通った中断点の名前（順序どおり）
    pub fn seen(&self) -> Vec<String> {
        self.0
            .as_ref()
            .map(|i| i.borrow().seen.clone())
            .unwrap_or_default()
    }
}
