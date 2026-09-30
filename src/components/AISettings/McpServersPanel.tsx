// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2024-2026 axpnet: AI-assisted (see AI-TRANSPARENCY.md)

import { useCallback, useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { Plus, RefreshCw, Save, Trash2 } from 'lucide-react';
import { createPortal } from 'react-dom';
import { ConfirmOverlay } from '../common/ConfirmOverlay';
import { MODAL_Z } from '../../utils/modalLayers';

interface SecretRef { vault_account: string }
interface ServerConfig {
    id: string;
    command: string;
    args: string[];
    env: Record<string, SecretRef>;
    enabled: boolean;
    revision: number;
}

const idValid = (value: string) => /^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$/.test(value);
const envValid = (value: string) => /^[A-Z_][A-Z0-9_]{0,63}$/.test(value)
    && !['PATH', 'HOME', 'SHELL', 'ENV', 'IFS', 'COMSPEC', 'PATHEXT', 'NODE_OPTIONS', 'NODE_PATH', 'RUSTFLAGS'].includes(value)
    && !/^(LD_|DYLD_|PYTHON)/.test(value);
const accountFor = (serverId: string, envName: string) => `mcp_env_${serverId.length}_${serverId}_${envName}`;

function ServerCard({ server, refresh }: { server: ServerConfig; refresh: () => Promise<void> }) {
    const [command, setCommand] = useState(server.command);
    const [args, setArgs] = useState(server.args.join('\n'));
    const [envName, setEnvName] = useState('');
    const [secret, setSecret] = useState('');
    const [busy, setBusy] = useState(false);
    const [error, setError] = useState('');
    const [pendingRemoval, setPendingRemoval] = useState<{ kind: 'server' } | { kind: 'secret'; name: string } | null>(null);

    const storedArgs = server.args.join('\n');
    useEffect(() => { setCommand(server.command); }, [server.command]);
    useEffect(() => { setArgs(storedArgs); }, [storedArgs]);

    const perform = async (action: () => Promise<void>) => {
        setBusy(true); setError('');
        try { await action(); await refresh(); }
        catch (cause) { setError(String(cause)); }
        finally { setBusy(false); }
    };

    const edit = (changes: Partial<ServerConfig>) => invoke<void>('mcp_client_upsert_server', {
        config: { ...server, ...changes, revision: server.revision + 1 },
    });

    const saveSecret = () => perform(async () => {
        try {
            if (!envValid(envName) || !secret) throw new Error('Enter an uppercase environment name and a secret.');
            if (!server.env[envName]) {
                await edit({ env: { ...server.env, [envName]: { vault_account: accountFor(server.id, envName) } } });
            }
            await invoke('mcp_client_set_secret', { serverId: server.id, envName, secret });
            setEnvName('');
        } finally { setSecret(''); }
    });

    return <div className="rounded-lg border border-gray-700 bg-gray-800 p-4 space-y-3">
        <div className="flex items-center justify-between gap-3">
            <div>
                <h3 className="font-medium text-white">{server.id}</h3>
                <p className="text-xs text-gray-400">Health: not connected · Tools: available after transport integration</p>
            </div>
            <div className="flex items-center gap-3">
                <label className="flex items-center gap-2 text-sm text-gray-300">
                    <input type="checkbox" checked={server.enabled} disabled={busy}
                        onChange={(event) => perform(() => edit({ enabled: event.target.checked }))} /> Enabled
                </label>
                <button type="button" disabled={busy} aria-label={`Remove ${server.id}`} className="text-red-400 disabled:opacity-50"
                    onClick={() => setPendingRemoval({ kind: 'server' })}><Trash2 size={16} /></button>
            </div>
        </div>
        <label className="block text-xs text-gray-300">Absolute executable path
            <input className="mt-1 w-full rounded bg-gray-900 border border-gray-600 p-2 text-sm" value={command}
                onChange={event => setCommand(event.target.value)} disabled={busy} />
        </label>
        <label className="block text-xs text-gray-300">Arguments (one literal argument per line)
            <textarea className="mt-1 w-full rounded bg-gray-900 border border-gray-600 p-2 text-sm" rows={2}
                value={args} onChange={event => setArgs(event.target.value)} disabled={busy} />
        </label>
        <button type="button" disabled={busy} className="flex items-center gap-1 rounded bg-purple-700 px-3 py-1.5 text-sm disabled:opacity-50"
            onClick={() => perform(() => edit({ command, args: args.split('\n').filter(Boolean) }))}><Save size={14} /> Save server</button>
        <div className="border-t border-gray-700 pt-3 space-y-2">
            <p className="text-xs text-gray-400">Secret environment variables stay in this user's encrypted vault. Saved values are never shown here.</p>
            {Object.keys(server.env).map(name => <div key={name} className="flex items-center gap-2 text-xs">
                <span className="font-mono text-gray-300">{name}</span><span className="text-gray-500">Saved value hidden</span>
                <button type="button" disabled={busy} className="text-red-400 disabled:opacity-50" aria-label={`Remove ${name}`}
                    onClick={() => setPendingRemoval({ kind: 'secret', name })}><Trash2 size={13} /></button>
            </div>)}
            <div className="flex flex-wrap gap-2">
                <input className="min-w-32 flex-1 rounded bg-gray-900 border border-gray-600 p-2 text-sm" value={envName}
                    placeholder="ENV_NAME" aria-label="Environment variable name" disabled={busy}
                    onChange={event => setEnvName(event.target.value)} />
                <input className="min-w-40 flex-1 rounded bg-gray-900 border border-gray-600 p-2 text-sm" value={secret}
                    type="password" autoComplete="new-password" placeholder="Secret value" aria-label="Secret value" disabled={busy}
                    onChange={event => setSecret(event.target.value)} />
                <button type="button" disabled={busy} className="rounded bg-gray-700 px-3 py-2 text-sm disabled:opacity-50"
                    onClick={saveSecret}>Save secret</button>
            </div>
        </div>
        {error && <p role="alert" className="text-xs text-red-400">{error}</p>}
        {pendingRemoval && createPortal(<ConfirmOverlay
            message={pendingRemoval.kind === 'server'
                ? `Remove ${server.id} and its saved MCP secrets?`
                : `Remove ${pendingRemoval.name} and its saved secret?`}
            onCancel={() => setPendingRemoval(null)}
            onConfirm={() => {
                const removal = pendingRemoval;
                setPendingRemoval(null);
                if (removal.kind === 'server') {
                    void perform(() => invoke('mcp_client_remove_server', { serverId: server.id }));
                } else {
                    void perform(() => {
                        const env = { ...server.env };
                        delete env[removal.name];
                        return edit({ env });
                    });
                }
            }}
            zClass={MODAL_Z.globalConfirm}
        />, document.body)}
    </div>;
}

export function McpServersPanel() {
    const [servers, setServers] = useState<ServerConfig[]>([]);
    const [id, setId] = useState('');
    const [command, setCommand] = useState('');
    const [busy, setBusy] = useState(false);
    const [error, setError] = useState('');
    const refreshSequence = useRef(0);
    const refresh = useCallback(async () => {
        const sequence = ++refreshSequence.current;
        const result = await invoke<ServerConfig[]>('mcp_client_list_servers');
        if (sequence === refreshSequence.current) setServers(result);
    }, []);
    useEffect(() => { void refresh().catch(cause => setError(String(cause))); }, [refresh]);

    const add = async () => {
        if (!idValid(id) || !command) { setError('Enter a valid server ID and absolute executable path.'); return; }
        setBusy(true); setError('');
        try {
            await invoke('mcp_client_upsert_server', { config: {
                id, command, args: [], env: {}, enabled: false, revision: 1,
            } satisfies ServerConfig });
            setId(''); setCommand(''); await refresh();
        } catch (cause) { setError(String(cause)); }
        finally { setBusy(false); }
    };

    return <div className="space-y-4">
        <div className="rounded-lg border border-amber-700/60 bg-amber-950/30 p-3 text-sm text-amber-200">
            MCP client execution is not available yet. Saving or enabling a server here does not start it or grant tool access.
        </div>
        <div className="flex items-center justify-between">
            <div><h2 className="font-medium text-white">MCP servers</h2><p className="text-xs text-gray-400">Manual STDIO configuration for the active user</p></div>
            <button type="button" onClick={() => void refresh().catch(cause => setError(String(cause)))} aria-label="Refresh MCP servers"
                className="text-gray-400 hover:text-white"><RefreshCw size={16} /></button>
        </div>
        <div className="rounded-lg border border-gray-700 p-3 space-y-2">
            <div className="flex flex-wrap gap-2">
                <input className="min-w-32 flex-1 rounded bg-gray-900 border border-gray-600 p-2 text-sm" value={id}
                    placeholder="Server ID" aria-label="Server ID" disabled={busy} onChange={event => setId(event.target.value)} />
                <input className="min-w-56 flex-[2] rounded bg-gray-900 border border-gray-600 p-2 text-sm" value={command}
                    placeholder="Absolute executable path" aria-label="Absolute executable path" disabled={busy}
                    onChange={event => setCommand(event.target.value)} />
                <button type="button" disabled={busy} onClick={() => void add()}
                    className="flex items-center gap-1 rounded bg-purple-700 px-3 py-2 text-sm disabled:opacity-50"><Plus size={14} /> Add server</button>
            </div>
            <p className="text-xs text-gray-500">Arguments are edited as literal values after adding a server. Shell commands and expansion are rejected.</p>
        </div>
        {servers.map(server => <ServerCard key={`${server.id}:${server.revision}`} server={server} refresh={refresh} />)}
        {servers.length === 0 && <p className="text-sm text-gray-500">No MCP servers configured for this user.</p>}
        {error && <p role="alert" className="text-xs text-red-400">{error}</p>}
    </div>;
}
