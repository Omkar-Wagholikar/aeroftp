// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2024-2026 axpnet: AI-assisted (see AI-TRANSPARENCY.md)

/** Reviewed Singapore Model Studio endpoints, including workspace-specific hosts. */
export function isModelStudioEndpoint(baseUrl: string): boolean {
    try {
        const url = new URL(baseUrl);
        return url.protocol === 'https:' && !url.username && !url.password && !url.port && !url.search && !url.hash
            && url.pathname.replace(/\/+$/, '') === '/compatible-mode/v1'
            && (url.hostname === 'dashscope-intl.aliyuncs.com'
                || /^[a-z0-9-]+\.ap-southeast-1\.maas\.aliyuncs\.com$/.test(url.hostname));
    } catch { return false; }
}

export const MODEL_STUDIO_MODELS = ['qwen3.8-flash', 'qwen3.8-max-0902', 'qwen3.8-2.4t-a95b', 'deepseek-v4-pro-0813', 'deepseek-v4.1-flash', 'kimi-k3'] as const;

export function usesModelStudioContract(type: string, baseUrl: string, model: string): boolean {
    return (type === 'qwen' || type === 'custom') && isModelStudioEndpoint(baseUrl)
        && MODEL_STUDIO_MODELS.some(id => id === model);
}
