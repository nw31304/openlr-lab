import React from 'react';

function renderInline(text) {
  // Split on **bold** markers
  const parts = text.split(/(\*\*[^*]+\*\*)/g);
  return parts.map((p, i) =>
    p.startsWith('**') && p.endsWith('**')
      ? <strong key={i}>{p.slice(2, -2)}</strong>
      : p
  );
}

export function renderLlmText(text) {
  // Simple inline renderer: bold (**x**), section labels (Word:), bullets (- x)
  return text.split('\n').map((line, i) => {
    const trimmed = line.trimStart();
    // Bullet line
    if (trimmed.startsWith('- ') || trimmed.startsWith('• ')) {
      return <div key={i} className="llm-bullet">{renderInline(trimmed.slice(2))}</div>;
    }
    // Section header: "Word word:" at start of line
    if (/^[A-Z][^:]{0,30}:\s*$/.test(trimmed)) {
      return <div key={i} className="llm-section-hdr">{trimmed.replace(/:$/, '')}</div>;
    }
    // Empty line
    if (!trimmed) return <div key={i} className="llm-spacer" />;
    return <div key={i}>{renderInline(line)}</div>;
  });
}
