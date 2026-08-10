use anyhow::{Result, bail};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentId {
    Codex,
    Claude,
    Agy,
}

impl AgentId {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude",
            Self::Agy => "Agy",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct AgentDefinition {
    pub id: AgentId,
    pub command: &'static str,
    pub role: &'static str,
}

pub const fn agents() -> [AgentDefinition; 3] {
    [
        AgentDefinition {
            id: AgentId::Codex,
            command: "codex",
            role: "builder",
        },
        AgentDefinition {
            id: AgentId::Claude,
            command: "claude",
            role: "reviewer",
        },
        AgentDefinition {
            id: AgentId::Agy,
            command: "agy",
            role: "specialist",
        },
    ]
}

pub fn handoff_text_from(from: &str, request: &str, context: &str) -> Result<String> {
    let request = request.trim();
    if request.is_empty() {
        bail!("handoff request cannot be empty");
    }
    let context = context.trim();
    Ok(format!(
        "[Agent Bridge handoff]\nSource: {}\nRequest: {}\n\nRecent source terminal:\n---\n{}\n---",
        from.trim(),
        request,
        if context.is_empty() {
            "(no visible source context)"
        } else {
            context
        }
    ))
}

pub fn session_title(agent: AgentId, ordinal: u32) -> String {
    format!("{} {ordinal}", agent.name())
}

#[derive(Debug, Default)]
pub struct TabSet<T> {
    items: Vec<T>,
    active: Option<usize>,
}

impl<T> TabSet<T> {
    pub const fn new() -> Self {
        Self {
            items: Vec::new(),
            active: None,
        }
    }

    pub fn push(&mut self, item: T) {
        self.items.push(item);
        self.active = Some(self.items.len() - 1);
    }

    pub fn remove_active(&mut self) -> Option<T> {
        let index = self.active?;
        self.remove(index)
    }

    pub fn remove(&mut self, index: usize) -> Option<T> {
        if index >= self.items.len() {
            return None;
        }
        let removed = self.items.remove(index);
        self.active = if self.items.is_empty() {
            None
        } else {
            self.active.map(|active| {
                if index < active {
                    active - 1
                } else {
                    active.min(self.items.len() - 1)
                }
            })
        };
        Some(removed)
    }

    pub fn replace_active(&mut self, item: T) -> Option<T> {
        let index = self.active?;
        Some(std::mem::replace(&mut self.items[index], item))
    }

    pub fn move_active(&mut self, delta: isize) {
        if let Some(active) = self.active {
            self.active =
                Some((active as isize + delta).rem_euclid(self.items.len() as isize) as usize);
        }
    }

    pub const fn active_index(&self) -> Option<usize> {
        self.active
    }

    pub fn set_active(&mut self, index: usize) -> bool {
        if index >= self.items.len() {
            return false;
        }
        self.active = Some(index);
        true
    }

    pub fn active(&self) -> Option<&T> {
        self.active.and_then(|index| self.items.get(index))
    }

    pub fn active_mut(&mut self) -> Option<&mut T> {
        self.active.and_then(|index| self.items.get_mut(index))
    }

    pub fn get(&self, index: usize) -> Option<&T> {
        self.items.get(index)
    }

    pub fn items(&self) -> &[T] {
        &self.items
    }

    pub fn items_mut(&mut self) -> &mut [T] {
        &mut self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}
