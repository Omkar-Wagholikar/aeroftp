// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2024-2026 axpnet: AI-assisted (see AI-TRANSPARENCY.md)

import { describe, expect, it, vi } from 'vitest';
import { registrySnapshots, runMcpTool, type McpBackendSnapshot, type McpToolCallContext } from './aiChatMcp';
import { buildToolRegistry } from './aiChatToolRegistry';

const ready: McpBackendSnapshot = {
    id: 'remote', transport: 'http', enabled: true, revision: 'r'.repeat(64), health: 'ready', errorCode: null,
    tools: [{ name: 'echo', description: null, schemaRevision: 'a'.repeat(64),
        inputSchema: { type: 'object', properties: { text: { type: 'string' } }, required: ['text'] } }],
    unsupportedTools: 0,
};

describe('MCP snapshots for the chat registry', () => {
    it('offers only ready servers and lets the registry validate every field', () => {
        const snapshots = registrySnapshots([
            ready,
            { ...ready, id: 'off', enabled: false, health: 'disabled', tools: [] },
            { ...ready, id: 'broken', health: 'error', errorCode: 'MCP_OAUTH_REQUIRED' },
            null,
        ]);
        expect(snapshots.map(server => server.id)).toEqual(['remote']);
        const mcp = buildToolRegistry([], [], snapshots).filter(entry => entry.source.kind === 'mcp');
        expect(mcp).toHaveLength(1);
        expect(mcp[0].source).toMatchObject({ ownerId: 'remote', transport: 'http', serverRevision: 'r'.repeat(64) });
        expect(registrySnapshots({ not: 'a list' })).toEqual([]);
    });
});

function context(overrides: Partial<McpToolCallContext> = {}): McpToolCallContext {
    return {
        source: { ownerId: 'remote', toolName: 'echo', transport: 'http', serverRevision: 'r'.repeat(64), schemaRevision: 'a'.repeat(64) },
        args: { text: 'hi' }, sessionId: 'chat-1', turnId: 'turn-1', skipNativeDialog: false,
        assertCurrent: () => undefined, serialize: run => run(), notApproved: 'not approved',
        ...overrides,
    };
}

describe('MCP tool call', () => {
    it('prepares, takes a one-shot grant and calls with the snapshot revisions', async () => {
        const invoke = vi.fn(async (command: string) => {
            if (command === 'mcp_client_tool_prepare') return { approvalRequired: true, requestId: 'req' };
            if (command === 'grant_ai_tool_approval') return { approved: true, grantId: 'grant' };
            return { content: [{ type: 'text', text: 'hi' }] };
        });
        let serialized = 0;
        const serialize = <T,>(run: () => Promise<T>) => { serialized += 1; return run(); };
        const result = await runMcpTool(invoke as never, context({ serialize, skipNativeDialog: true }));
        expect(result).toEqual({ content: [{ type: 'text', text: 'hi' }] });
        expect(serialized).toBe(1);
        const call = { transport: 'http', serverId: 'remote', toolName: 'echo', arguments: { text: 'hi' },
            expectedRevision: 'r'.repeat(64), expectedSchemaRevision: 'a'.repeat(64), sessionId: 'chat-1' };
        expect(invoke.mock.calls).toEqual([
            ['mcp_client_tool_prepare', { call }],
            ['grant_ai_tool_approval', { requestId: 'req', rememberForSession: false, skipNativeDialog: true }],
            ['mcp_client_tool_call', { call: { ...call, approvalGrantId: 'grant' }, turnId: 'turn-1' }],
        ]);
    });

    it('never calls without a grant, and stops when the entry goes stale during approval', async () => {
        const denied = vi.fn(async (command: string) => command === 'mcp_client_tool_prepare'
            ? { approvalRequired: true, requestId: 'req' } : { approved: false, grantId: null });
        await expect(runMcpTool(denied as never, context())).rejects.toThrow('not approved');
        expect(denied.mock.calls.map(([command]) => command)).not.toContain('mcp_client_tool_call');

        const noRequest = vi.fn(async () => ({ approvalRequired: false, requestId: null }));
        await expect(runMcpTool(noRequest as never, context())).rejects.toThrow('MCP_APPROVAL_REQUIRED');
        expect(noRequest).toHaveBeenCalledTimes(1);

        let stale = false;
        const granting = vi.fn(async (command: string) => {
            if (command === 'mcp_client_tool_prepare') return { approvalRequired: true, requestId: 'req' };
            stale = true;
            return { approved: true, grantId: 'grant' };
        });
        const assertCurrent = () => { if (stale) throw new Error('identity changed'); };
        await expect(runMcpTool(granting as never, context({ assertCurrent }))).rejects.toThrow('identity changed');
        expect(granting.mock.calls.map(([command]) => command)).not.toContain('mcp_client_tool_call');
    });
});
