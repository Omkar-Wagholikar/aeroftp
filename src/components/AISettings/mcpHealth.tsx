// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2024-2026 axpnet: AI-assisted (see AI-TRANSPARENCY.md)

import { createContext, useCallback, useContext, useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useTranslation } from '../../i18n';
import { MCP_SERVERS_CHANGED, type McpBackendSnapshot } from '../DevTools/aiChatMcp';
import { describeMcpError } from './mcpErrors';

export interface McpHealth {
    snapshots: ReadonlyMap<string, McpBackendSnapshot>;
    checking: boolean;
    /** `refresh` lists again even when the backend still holds a listing. */
    check: (refresh: boolean) => Promise<void>;
}

const key = (transport: string, id: string) => `${transport}:${id}`;
const McpHealthContext = createContext<McpHealth | null>(null);
export const McpHealthProvider = McpHealthContext.Provider;

export function useMcpHealthState(): McpHealth {
    const [snapshots, setSnapshots] = useState<ReadonlyMap<string, McpBackendSnapshot>>(new Map());
    const [checking, setChecking] = useState(false);
    const sequence = useRef(0);
    const check = useCallback(async (refresh: boolean) => {
        const current = ++sequence.current;
        setChecking(true);
        try {
            const result = await invoke<McpBackendSnapshot[]>('mcp_client_tool_snapshots', { refresh });
            if (current !== sequence.current) return;
            setSnapshots(new Map(result.map(server => [key(server.transport, server.id), server])));
        } catch {
            // The server lists report a locked or missing store themselves.
            if (current === sequence.current) setSnapshots(new Map());
        } finally {
            if (current === sequence.current) setChecking(false);
        }
    }, []);
    useEffect(() => {
        void check(false);
        const changed = () => { void check(false); };
        window.addEventListener(MCP_SERVERS_CHANGED, changed);
        return () => { sequence.current += 1; window.removeEventListener(MCP_SERVERS_CHANGED, changed); };
    }, [check]);
    return { snapshots, checking, check };
}

/** Live state of one server: never started while disabled, tools when ready. */
export function McpHealthLine({ transport, id }: { transport: 'stdio' | 'http'; id: string }) {
    const t = useTranslation();
    const health = useContext(McpHealthContext);
    const server = health?.snapshots.get(key(transport, id));
    if (!health || !server) {
        return <p className="text-xs text-gray-400" role="status">{health?.checking ? t('ai.mcpClient.healthChecking') : ''}</p>;
    }
    if (server.health === 'disabled') return <p className="text-xs text-gray-400" role="status">{t('ai.mcpClient.healthDisabled')}</p>;
    if (server.health === 'error') {
        return <p className="text-xs text-amber-300" role="status">
            {t('ai.mcpClient.healthError', { reason: describeMcpError(t, server.errorCode ?? '') })}</p>;
    }
    return <div className="text-xs text-gray-400" role="status">
        <p className="text-green-400">{t('ai.mcpClient.healthReady', { count: server.tools.length })}
            {server.unsupportedTools > 0 && <span className="text-amber-300">{' · '}{t('ai.mcpClient.healthUnsupported', { count: server.unsupportedTools })}</span>}</p>
        {server.tools.length > 0 && <details>
            <summary className="cursor-pointer">{t('ai.mcpClient.healthShowTools')}</summary>
            <ul className="mt-1 space-y-0.5">{server.tools.map(tool => <li key={tool.name}>
                <span className="font-mono text-gray-200">{tool.name}</span>{tool.description ? ` · ${tool.description}` : ''}</li>)}</ul>
        </details>}
    </div>;
}
