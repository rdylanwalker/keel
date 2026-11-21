// Client-side Mermaid rendering for the Keel mdBook site.
//
// mdBook renders fenced ```mermaid blocks as <pre><code class="language-mermaid">.
// This script swaps each block for a <pre class="mermaid"> node holding the
// diagram source, then renders it with the vendored Mermaid build using a theme
// that matches the active mdBook theme. Switching mdBook themes re-renders.
//
// Mermaid is large (~5 MB), so it is loaded lazily from docs/assets/js/ only on
// pages that actually contain a diagram.

(() => {
    const darkThemes = ['ayu', 'navy', 'coal'];
    const html = document.documentElement;

    const mermaidTheme = () =>
        darkThemes.some((theme) => html.classList.contains(theme)) ? 'dark' : 'default';

    const nodes = [];
    for (const code of document.querySelectorAll('pre > code.language-mermaid')) {
        const pre = code.parentElement;
        const diagram = document.createElement('pre');
        diagram.className = 'mermaid';
        diagram.dataset.source = code.textContent;
        diagram.textContent = code.textContent;
        pre.replaceWith(diagram);
        nodes.push(diagram);
    }
    if (nodes.length === 0) {
        return;
    }

    const root = typeof path_to_root === 'string' ? path_to_root : '';
    const loadMermaid = () =>
        new Promise((resolve, reject) => {
            if (window.mermaid) {
                resolve();
                return;
            }
            const script = document.createElement('script');
            script.src = `${root}assets/js/mermaid.min.js`;
            script.onload = () => resolve();
            script.onerror = () => reject(new Error(`failed to load ${script.src}`));
            document.head.appendChild(script);
        });

    let renderedTheme = null;
    const render = async () => {
        const theme = mermaidTheme();
        if (theme === renderedTheme) {
            return;
        }
        renderedTheme = theme;
        for (const node of nodes) {
            node.removeAttribute('data-processed');
            node.textContent = node.dataset.source;
        }
        mermaid.initialize({ startOnLoad: false, theme, securityLevel: 'strict' });
        try {
            await mermaid.run({ nodes });
        } catch (err) {
            console.error('Mermaid rendering failed', err);
        }
    };

    loadMermaid()
        .then(() => {
            render();
            new MutationObserver(render).observe(html, {
                attributes: true,
                attributeFilter: ['class'],
            });
        })
        .catch((err) => console.error('Mermaid could not be loaded', err));
})();
