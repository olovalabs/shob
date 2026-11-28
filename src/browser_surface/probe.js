(mode, target) => {
    if (location.protocol !== 'http:' && location.protocol !== 'https:') return {error: 'unavailable'};
    let node;
    try {
        node = typeof target === 'string' ? document.querySelector(target) :
            target === null ? document.activeElement : document.elementFromPoint(target[0], target[1]);
    } catch {
        return {error: 'invalid-selector'};
    }

    const visiblePoint = element => {
        if (!element?.checkVisibility({opacityProperty: true, visibilityProperty: true, contentVisibilityAuto: true})) return null;
        const rect = element.getBoundingClientRect();
        const left = Math.max(0, rect.left);
        const top = Math.max(0, rect.top);
        const right = Math.min(innerWidth, rect.right);
        const bottom = Math.min(innerHeight, rect.bottom);
        if (left >= right || top >= bottom) return null;
        const x = Math.min(innerWidth - 1, Math.round((left + right) / 2));
        const y = Math.min(innerHeight - 1, Math.round((top + bottom) / 2));
        return element.contains(document.elementFromPoint(x, y)) ? [x, y] : null;
    };

    const point = visiblePoint(node);
    if (mode === 'wait') return point !== null;
    if (mode === 'point') return point ?? {error: 'unavailable'};
    if (target !== null && !point) return {error: 'unavailable'};

    const field = node?.closest('input, textarea, [contenteditable]');
    if (!field || field.matches(':disabled, [readonly]') ||
        (field.localName === 'input' && !['text', 'search', 'password', 'url', 'tel', 'email', 'number'].includes(field.type)) ||
        (!field.matches('input, textarea') && !field.isContentEditable)) {
        return {error: 'not-editable'};
    }
    if (target !== null) field.focus({preventScroll: true});
    return field === document.activeElement ? true : {error: 'not-editable'};
}
