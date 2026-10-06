#[derive(Clone, Debug)]
pub struct History<T> {
    entries: Vec<T>,
    undone: Vec<T>,
}

impl<T> Default for History<T> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            undone: Vec::new(),
        }
    }
}

impl<T> History<T> {
    pub fn push(&mut self, entry: T) {
        self.entries.push(entry);
        self.undone.clear();
    }

    pub fn undo(&mut self) -> bool {
        let Some(entry) = self.entries.pop() else {
            return false;
        };
        self.undone.push(entry);
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(entry) = self.undone.pop() else {
            return false;
        };
        self.entries.push(entry);
        true
    }

    pub fn entries(&self) -> &[T] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn undo_and_redo_move_entries_between_stacks() {
        let mut history = History::default();
        history.push(1);
        history.push(2);
        assert!(history.undo());
        assert_eq!(history.entries(), &[1]);
        assert!(history.redo());
        assert_eq!(history.entries(), &[1, 2]);
    }

    #[test]
    fn new_entry_clears_redo_stack() {
        let mut history = History::default();
        history.push(1);
        history.push(2);
        history.undo();
        history.push(3);
        assert!(!history.redo());
        assert_eq!(history.entries(), &[1, 3]);
    }
}
