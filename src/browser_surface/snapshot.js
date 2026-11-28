(() => {
    if (location.protocol !== 'http:' && location.protocol !== 'https:') return null;
    const body = document.body;
    const width = innerWidth;
    const height = innerHeight;
    const output = {text: '', targets: [], truncated: false};
    if (!body) return output;

    const visibleBox = rect => rect.width > 0 && rect.height > 0 &&
        rect.right > 0 && rect.left < width && rect.bottom > 0 && rect.top < height;
    const visible = element => element.checkVisibility({
        opacityProperty: true,
        visibilityProperty: true,
        contentVisibilityAuto: true,
    });
    const selector = element => {
        const parts = [];
        for (let node = element; node && parts.length < 10; node = node.parentElement) {
            if (node === document.documentElement) {
                parts.unshift('html');
                return parts.join(' > ').slice(0, 240);
            }
            if (node.id && node.id.length <= 64) {
                const id = '#' + CSS.escape(node.id);
                const matches = document.querySelectorAll(id);
                if (matches.length === 1 && matches[0] === node) {
                    parts.unshift(id);
                    const path = parts.join(' > ');
                    return path.length <= 240 ? path : null;
                }
            }
            let nth = 1;
            for (let sibling = node.previousElementSibling; sibling; sibling = sibling.previousElementSibling) {
                if (++nth > 256) return null;
            }
            parts.unshift(`:nth-child(${nth})`);
        }
        return null;
    };

    const controls = 'a[href],button,input:not([type=hidden]),select,textarea,summary,label,[role=button],[role=link],[role=menuitem],[tabindex],[contenteditable]';
    const walker = document.createTreeWalker(body, NodeFilter.SHOW_ELEMENT | NodeFilter.SHOW_TEXT, {
        acceptNode(node) {
            return node.nodeType === Node.ELEMENT_NODE && node.matches('script,style,noscript,template')
                ? NodeFilter.FILTER_REJECT : NodeFilter.FILTER_ACCEPT;
        },
    });
    const range = document.createRange();
    let remaining = 4500;
    for (let i = 0, node; (node = walker.nextNode()); ++i) {
        if (i >= 2400) {
            output.truncated = true;
            break;
        }
        if (node.nodeType === Node.TEXT_NODE && remaining > 0) {
            const count = Math.min(node.length, 256, remaining - 1);
            if (count < 1 || !visible(node.parentElement)) continue;
            const text = node.substringData(0, count).trim();
            if (!text) continue;
            range.setStart(node, 0);
            range.setEnd(node, count);
            if (!visibleBox(range.getBoundingClientRect())) continue;
            if (count < node.length) output.truncated = true;
            output.text += text + '\n';
            remaining -= text.length + 1;
        } else if (node.nodeType === Node.ELEMENT_NODE && node.matches(controls)) {
            if (!visible(node)) continue;
            const r = node.getBoundingClientRect();
            if (!visibleBox(r)) continue;
            if (output.targets.length >= 40) {
                output.truncated = true;
                continue;
            }
            const x = Math.round((Math.max(0, r.left) + Math.min(width - 1, r.right)) / 2);
            const y = Math.round((Math.max(0, r.top) + Math.min(height - 1, r.bottom)) / 2);
            const hit = document.elementFromPoint(x, y);
            if (!node.contains(hit)) continue;
            const label = node.getAttribute('aria-label') || node.getAttribute('placeholder') ||
                node.getAttribute('title') || (node.firstChild?.nodeType === Node.TEXT_NODE
                    ? node.firstChild.substringData(0, 80).trim() : '');
            output.targets.push({tag: node.localName.slice(0, 32), label: label.slice(0, 80),
                selector: selector(node), x, y});
        }
    }
    if (remaining <= 1) output.truncated = true;
    return output;
})()
