pub(crate) fn push_unique<T: Eq>(items: &mut Vec<T>, value: T) {
    if !items.iter().any(|existing| existing == &value) {
        items.push(value);
    }
}

pub(crate) fn extend_unique<T: Eq>(items: &mut Vec<T>, additions: impl IntoIterator<Item = T>) {
    for addition in additions {
        push_unique(items, addition);
    }
}
