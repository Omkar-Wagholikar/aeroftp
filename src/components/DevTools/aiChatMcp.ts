// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2024-2026 axpnet: AI-assisted (see AI-TRANSPARENCY.md)

import type { McpServerSnapshot, McpTransport } from './aiChatToolRegistry';

/** One server as `mcp_client_tool_snapshots` reports it. Untrusted discovery data. */
export interface McpBackendSnapshot {
    id: string;
    transport: McpTransport;
    enabled: boolean;
    revision: string;
    health: 'disabled' | 'ready' | 'error';
    errorCode: string | null;
    tools: { name: string; description: string | null; inputSchema: unknown; schemaRevision: string }[];
    unsupportedTools: number;
}

/** Settings and chat reload their snapshots when a server changes. */
export const MCP_SERVERS_CHANGED = 'aeroftp:mcp-servers-changed';
export const notifyMcpServersChanged = () => window.dispatchEvent(new Event(MCP_SERVERS_CHANGED));

/** Only a ready server offers tools to the model; the registry validates the rest. */
export function registrySnapshots(snapshots: unknown): McpServerSnapshot[] {
    if (!Array.isArray(snapshots)) return [];
    return (snapshots as Partial<McpBackendSnapshot>[])
        .filter(server => server && server.health === 'ready' && server.enabled === true && Array.isArray(server.tools))
        .map(server => ({
            id: server.id as string,
            transport: server.transport as McpTransport,
            revision: server.revision as string,
            enabled: true,
            tools: server.tools!.map(tool => ({
                name: tool?.name,
                description: tool?.description ?? undefined,
                inputSchema: tool?.inputSchema,
                schemaRevision: tool?.schemaRevision,
                enabled: true,
            }) as McpServerSnapshot['tools'][number]),
        }));
}

type Invoke = <T>(command: string, args?: Record<string, unknown>) => Promise<T>;
interface Preparation { approvalRequired: boolean; requestId?: string | null }
interface Grant { approved: boolean; grantId?: string | null }

export interface McpToolCallContext {
    source: { ownerId: string; toolName: string; transport: McpTransport; serverRevision: string; schemaRevision: string };
    args: Record<string, unknown>;
    sessionId?: string;
    turnId?: string;
    /** The panel already confirmed the call in expert mode. */
    skipNativeDialog: boolean;
    /** Throws when the turn ended or the registry entry changed. */
    assertCurrent: () => void;
    /** Serializes backend approval windows. */
    serialize: <T>(run: () => Promise<T>) => Promise<T>;
    notApproved: string;
}

/** Every MCP call needs its own approval: the grant is never remembered for
 *  the chat, and the call names the snapshot revisions it was offered with. */
export async function runMcpTool(invoke: Invoke, context: McpToolCallContext): Promise<unknown> {
    const { source } = context;
    const call = {
        transport: source.transport,
        serverId: source.ownerId,
        toolName: source.toolName,
        arguments: context.args,
        expectedRevision: source.serverRevision,
        expectedSchemaRevision: source.schemaRevision,
        sessionId: context.sessionId,
    };
    return context.serialize(async () => {
        context.assertCurrent();
        const preparation = await invoke<Preparation>('mcp_client_tool_prepare', { call });
        if (!preparation.approvalRequired || !preparation.requestId) throw new Error('MCP_APPROVAL_REQUIRED');
        context.assertCurrent();
        const grant = await invoke<Grant>('grant_ai_tool_approval', {
            requestId: preparation.requestId,
            rememberForSession: false,
            skipNativeDialog: context.skipNativeDialog,
        });
        if (!grant.approved || !grant.grantId) throw new Error(context.notApproved);
        context.assertCurrent();
        return invoke('mcp_client_tool_call', { call: { ...call, approvalGrantId: grant.grantId }, turnId: context.turnId });
    });
}
