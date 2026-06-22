import React, { useState, useEffect } from 'react';
import { useStore } from '../store.js';
import { PROVIDERS, testConnection } from '../llmClient.js';

export default function LlmSettingsPanel() {
  const { llmConfig, setLlmConfig, clearLlmConfig, showLlmSettings, toggleLlmSettings } = useStore();
  const [draft, setDraft] = useState(() => initDraft(llmConfig));
  const [showKey, setShowKey] = useState(false);
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState(null); // { ok, text }

  // Reset draft when panel opens
  useEffect(() => {
    if (showLlmSettings) {
      setDraft(initDraft(llmConfig));
      setTestResult(null);
      setShowKey(false);
    }
  }, [showLlmSettings]); // eslint-disable-line react-hooks/exhaustive-deps

  if (!showLlmSettings) return null;

  const provider = PROVIDERS.find(p => p.id === draft.providerId) ?? PROVIDERS[0];
  const hasKey   = provider.authStyle !== 'none';
  const isValid  = draft.baseUrl.trim() && draft.model.trim() && (!hasKey || draft.apiKey.trim());

  function handleProviderChange(id) {
    const p = PROVIDERS.find(pr => pr.id === id);
    setDraft(d => ({
      ...d,
      providerId: id,
      baseUrl: p.baseUrl,
      model: p.defaultModel,
      apiKey: id === draft.providerId ? d.apiKey : '',
    }));
    setTestResult(null);
  }

  async function handleTest() {
    setTesting(true);
    setTestResult(null);
    const result = await testConnection(draft);
    setTesting(false);
    setTestResult(result.ok
      ? { ok: true,  text: `Connected — model replied: "${(result.content ?? '').slice(0, 60)}"` }
      : { ok: false, text: result.error ?? 'Unknown error' }
    );
  }

  function handleSave() {
    setLlmConfig({ ...draft });
    toggleLlmSettings();
  }

  function handleClear() {
    clearLlmConfig();
    setDraft(initDraft(null));
    setTestResult(null);
  }

  return (
    <div className="llm-settings-panel">
      <div className="params-panel-header">
        <span className="params-panel-title">AI / LLM Settings</span>
        <button className="params-panel-close" onClick={toggleLlmSettings} title="Close">✕</button>
      </div>
      <div className="params-panel-body llm-body">

        <label className="llm-row">
          <span className="llm-label">Provider</span>
          <select
            className="llm-select"
            value={draft.providerId}
            onChange={e => handleProviderChange(e.target.value)}
          >
            {PROVIDERS.map(p => (
              <option key={p.id} value={p.id}>{p.label}</option>
            ))}
          </select>
        </label>

        {provider.note && (
          <div className="llm-note">{provider.note}</div>
        )}

        <label className="llm-row">
          <span className="llm-label">Base URL</span>
          <input
            className="llm-input"
            type="url"
            value={draft.baseUrl}
            onChange={e => setDraft(d => ({ ...d, baseUrl: e.target.value }))}
            spellCheck={false}
          />
        </label>

        <label className="llm-row">
          <span className="llm-label">Model</span>
          <input
            className="llm-input"
            type="text"
            value={draft.model}
            onChange={e => setDraft(d => ({ ...d, model: e.target.value }))}
            spellCheck={false}
            placeholder={provider.defaultModel}
          />
        </label>

        {hasKey && (
          <label className="llm-row">
            <span className="llm-label">API Key</span>
            <div className="llm-key-wrap">
              <input
                className="llm-input llm-key-input"
                type={showKey ? 'text' : 'password'}
                value={draft.apiKey}
                onChange={e => setDraft(d => ({ ...d, apiKey: e.target.value }))}
                autoComplete="off"
                placeholder="sk-…"
              />
              <button
                className="llm-key-toggle"
                onClick={() => setShowKey(s => !s)}
                title={showKey ? 'Hide key' : 'Show key'}
                type="button"
              >{showKey ? '🙈' : '👁'}</button>
            </div>
          </label>
        )}

        <div className="llm-storage-note">
          Key is stored in browser localStorage — never sent anywhere except the configured provider URL.
        </div>

        <div className="llm-actions">
          <button
            className="llm-btn llm-test-btn"
            onClick={handleTest}
            disabled={testing || !isValid}
          >{testing ? 'Testing…' : 'Test connection'}</button>
          <button
            className="llm-btn llm-save-btn"
            onClick={handleSave}
            disabled={!isValid}
          >Save</button>
          {llmConfig && (
            <button
              className="llm-btn llm-clear-btn"
              onClick={handleClear}
            >Clear</button>
          )}
        </div>

        {testResult && (
          <div className={`llm-test-result ${testResult.ok ? 'ok' : 'err'}`}>
            {testResult.ok ? '✓' : '✗'} {testResult.text}
          </div>
        )}
      </div>
    </div>
  );
}

function initDraft(config) {
  if (config) return { ...config };
  const p = PROVIDERS[0];
  return { providerId: p.id, baseUrl: p.baseUrl, model: p.defaultModel, apiKey: '' };
}
