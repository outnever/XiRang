//! 编辑层：即时追加落盘（append-v1）+ 完整撤销栈。
//!
//! 每次编辑 = 往文件末尾追加一条记录（同编号 = 修订，新编号 = 新增），毫秒级；
//! 撤销 / 重做同样以追加记录表达，所以整个会话的历史都留在文件里，随时可回滚。

use std::collections::HashSet;
use std::path::Path;

use xirang_core::codec::{Node, Uuid, Value};
use xirang_core::index::AppendWriter;
use xirang_core::tree::{self};

/// 一次编辑的前后状态（撤销 = 追加「前」态，重做 = 追加「后」态）。
#[derive(Clone, Debug)]
pub struct Edit {
    pub id: Uuid,
    pub parent: Option<Uuid>,
    pub before_name: String,
    pub before_value: Value,
    pub after_name: String,
    pub after_value: Value,
    /// 新建出来的节点（撤销 = 追加一条空记录，把槽位清掉）。
    pub created: bool,
    /// 所属根的编号（首次编辑时要在根下挂 `@protocol = append-v1`）。
    pub root: Option<Uuid>,
}

impl Edit {
    fn to_node(&self, after: bool, empty: bool) -> Node {
        if empty {
            return Node {
                id: self.id,
                parent: self.parent,
                name: String::new(),
                value: Value::Empty,
            };
        }
        Node {
            id: self.id,
            parent: self.parent,
            name: if after {
                self.after_name.clone()
            } else {
                self.before_name.clone()
            },
            value: if after {
                self.after_value.clone()
            } else {
                self.before_value.clone()
            },
        }
    }
}

pub struct Editor {
    writer: AppendWriter,
    undo_stack: Vec<Edit>,
    redo_stack: Vec<Edit>,
    marked: HashSet<Uuid>,
    /// 本次会话追加的记录条数（关闭文件时用来提示是否合并）。
    pub appended: usize,
}

impl Editor {
    pub fn open(path: &Path) -> Result<Editor, String> {
        let (writer, _report) = AppendWriter::open(path)?;
        Ok(Editor {
            writer,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            marked: HashSet::new(),
            appended: 0,
        })
    }

    /// 首次在某根下编辑时，挂 `@protocol = append-v1`（重复挂无害）。
    fn ensure_marker(&mut self, root: Option<Uuid>) -> Result<(), String> {
        let Some(root) = root else { return Ok(()) };
        if !self.marked.insert(root) {
            return Ok(());
        }
        let marker = Node {
            id: Uuid::random_v4(),
            parent: Some(root),
            name: "@protocol".into(),
            value: Value::Text(tree::PROTOCOL_APPEND.into()),
        };
        self.writer.append_node(&marker)?;
        self.appended += 1;
        Ok(())
    }

    /// 执行一次编辑并立即落盘。
    pub fn apply(&mut self, edit: Edit) -> Result<(), String> {
        self.ensure_marker(edit.root)?;
        self.writer.append_node(&edit.to_node(true, false))?;
        self.appended += 1;
        self.writer.sync()?;
        self.redo_stack.clear();
        self.undo_stack.push(edit);
        Ok(())
    }

    pub fn can_undo(&self) -> bool {
        !self.undo_stack.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo_stack.is_empty()
    }

    /// 撤销：追加「前态」记录。
    pub fn undo(&mut self) -> Result<bool, String> {
        let Some(edit) = self.undo_stack.pop() else {
            return Ok(false);
        };
        self.writer.append_node(&edit.to_node(false, edit.created))?;
        self.appended += 1;
        self.writer.sync()?;
        self.redo_stack.push(edit);
        Ok(true)
    }

    /// 重做：追加「后态」记录。
    pub fn redo(&mut self) -> Result<bool, String> {
        let Some(edit) = self.redo_stack.pop() else {
            return Ok(false);
        };
        self.writer.append_node(&edit.to_node(true, false))?;
        self.appended += 1;
        self.writer.sync()?;
        self.undo_stack.push(edit);
        Ok(true)
    }

}

/// 改值：老值 → 新值。
pub fn set_value(node: &Node, value: Value, root: Option<Uuid>) -> Edit {
    Edit {
        id: node.id,
        parent: node.parent,
        before_name: node.name.clone(),
        before_value: node.value.clone(),
        after_name: node.name.clone(),
        after_value: value,
        created: false,
        root,
    }
}

/// 改名。
pub fn rename(node: &Node, name: String, root: Option<Uuid>) -> Edit {
    Edit {
        id: node.id,
        parent: node.parent,
        before_name: node.name.clone(),
        before_value: node.value.clone(),
        after_name: name,
        after_value: node.value.clone(),
        created: false,
        root,
    }
}

/// 新增子节点。
pub fn create(parent: Option<Uuid>, name: String, value: Value, root: Option<Uuid>) -> Edit {
    Edit {
        id: Uuid::random_v4(),
        parent,
        before_name: String::new(),
        before_value: Value::Empty,
        after_name: name,
        after_value: value,
        created: true,
        root,
    }
}

/// 删除 = 追加一条同编号、名字与值都为空的记录（留空槽位，编号不消失）。
pub fn delete(node: &Node, root: Option<Uuid>) -> Edit {
    Edit {
        id: node.id,
        parent: node.parent,
        before_name: node.name.clone(),
        before_value: node.value.clone(),
        after_name: String::new(),
        after_value: Value::Empty,
        created: false,
        root,
    }
}
