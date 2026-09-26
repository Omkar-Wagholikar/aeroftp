// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2024-2026 axpnet: AI-assisted (see AI-TRANSPARENCY.md)

import { describe, expect, it } from 'vitest';
import type { AIModel, AIProvider } from './ai';
import { normalizeModelCatalog, providerModelSnapshot, reconcileProviderNames, resolveProviderModel } from './aiModelDiscovery';
import { isModelStudioEndpoint, MODEL_STUDIO_MODELS } from './aiModelStudio';
import { requiresNativeTurn } from '../components/DevTools/aiChatNativeTurn';
import { resolveModelContext } from './aiModelRegistry';

const nvidia = { id: 'n', type: 'nvidia', baseUrl: 'https://integrate.api.nvidia.com/v1' } as AIProvider;
const router = { id: 'r', type: 'openrouter', baseUrl: 'https://openrouter.ai/api/v1' } as AIProvider;
describe('provider capability discovery', () => {
    it('renames only the legacy preset label without changing credentials or endpoint', () => {
        const preset = { ...nvidia, type: 'qwen', name: 'Qwen (Alibaba)' } as AIProvider;
        const customName = { ...preset, name: 'My Singapore account' };
        expect(reconcileProviderNames([preset, customName])).toEqual([{ ...preset, name: 'Alibaba Model Studio' }, customName]);
    });
    it('recognizes all six Model Studio deployments for presets and workspace Custom providers', () => {
        for (const type of ['qwen', 'custom'] as const) {
            const provider = { id: 'a', type, baseUrl: 'https://llm-test.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1' } as AIProvider;
            for (const name of MODEL_STUDIO_MODELS) {
                const model = resolveProviderModel({ name, supportsTools: false }, provider);
                expect(model).toMatchObject({ supportsTools: true, supportsThinking: true, capabilitySource: 'provider' });
                expect(model.supportsVision).toBe(!['qwen3.8-2.4t-a95b', 'deepseek-v4-pro-0813'].includes(name));
                expect(model.nativeCapabilities).toBeUndefined();
                expect(requiresNativeTurn({ provider_type: type, base_url: provider.baseUrl, model: name })).toBe(true);
            }
        }
    });
    it('does not apply the Singapore contract to lookalikes, other regions or unknown models', () => {
        expect(isModelStudioEndpoint('https://dashscope-intl.aliyuncs.com/compatible-mode/v1/')).toBe(true);
        for (const baseUrl of ['https://dashscope-intl.aliyuncs.com.evil.test/compatible-mode/v1', 'http://dashscope-intl.aliyuncs.com/compatible-mode/v1', 'https://user@dashscope-intl.aliyuncs.com/compatible-mode/v1', 'https://dashscope-intl.aliyuncs.com/compatible-mode/v1?route=other', 'https://dashscope-intl.aliyuncs.com:8443/compatible-mode/v1', 'https://llm-test.cn-beijing.maas.aliyuncs.com/compatible-mode/v1', 'https://nested.llm-test.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1']) {
            expect(isModelStudioEndpoint(baseUrl)).toBe(false);
            expect(providerModelSnapshot({ ...nvidia, type: 'custom', baseUrl }, 'kimi-k3')).toBeUndefined();
            expect(requiresNativeTurn({ provider_type: 'custom', base_url: baseUrl, model: 'kimi-k3' })).toBe(false);
        }
        expect(providerModelSnapshot({ ...nvidia, type: 'qwen', baseUrl: 'https://dashscope-intl.aliyuncs.com/compatible-mode/v1' }, 'unknown')).toBeUndefined();
    });
    it('displays both names-only and enriched catalogs and rejects malformed rows', () => {
        expect(normalizeModelCatalog(['vendor/model', { id: 'vendor/other', supportsTools: true }, null, {}, '', { id: 3 }])).toEqual([{ id: 'vendor/model' }, { id: 'vendor/other', supportsTools: true }]);
        expect(normalizeModelCatalog({ data: [] })).toEqual([]);
        expect(normalizeModelCatalog([{ id: 'safe', supportsTools: 'true', maxTokens: -1 }])).toEqual([{ id: 'safe' }]);
    });
    it('corrects guessed NVIDIA flags and differentiates text from vision endpoints', () => {
        for (const [name, vision] of [['nvidia/nemotron-3-ultra-550b-a55b', false], ['z-ai/glm-5.3', false], ['z-ai/glm-5.3-flash', true], ['moonshotai/kimi-k3', true], ['deepseek-ai/deepseek-v4.1-flash', true]] as const) {
            const model = resolveProviderModel({ name, supportsVision: true, supportsTools: false }, nvidia);
            expect(model).toMatchObject({ supportsVision: vision, supportsTools: true, supportsThinking: true, capabilitySource: 'provider' });
            expect(model.nativeCapabilities).toBeUndefined();
        }
    });
    it('reads exact router IDs including free variants from catalog metadata', () => {
        const model = resolveProviderModel({ name: 'new/model:free' }, router, { id: 'new/model:free', supportsTools: true, supportsVision: false, maxContextTokens: 32000 });
        expect(model).toMatchObject({ supportsTools: true, supportsVision: false, supportsThinking: false, maxContextTokens: 32000, capabilitySource: 'provider' });
        expect(resolveProviderModel(model as AIModel, router)).toEqual(model);
    });
    it('never transfers a snapshot to another endpoint, provider, or renamed model', () => {
        const model = resolveProviderModel({ name: 'new/model:free' }, router, { id: 'new/model:free', supportsTools: true });
        for (const [m, p] of [[model, { ...router, baseUrl: 'https://private.example/v1' }], [{ ...model, name: 'different' }, router], [model, { ...router, type: 'custom' }]] as const) {
            const resolved = resolveProviderModel(m as AIModel, p as AIProvider);
            expect(resolved.supportsTools).toBe(false);
            expect(resolved.capabilitySource).toBe('unknown');
        }
        expect(providerModelSnapshot({ ...nvidia, type: 'custom' }, 'moonshotai/kimi-k3')).toBeUndefined();
    });
    it('drops automatic token ceilings when leaving the endpoint that established them', () => {
        const info = { id: 'vendor/model', supportsTools: true, maxContextTokens: 1000000, maxTokens: 2048 };
        const model = resolveProviderModel({ name: info.id }, router, info) as AIModel;
        const moved = resolveProviderModel(model, { ...router, baseUrl: 'https://private.example/v1' });
        expect(moved.maxContextTokens).toBeUndefined();
        expect(resolveModelContext(moved).tokens).toBeLessThan(1000000);
        expect(moved.maxTokens).not.toBe(2048);
        const explicit = resolveProviderModel({ name: info.id, maxContextTokens: 16000, maxTokens: 1000 }, router, info) as AIModel;
        expect(resolveProviderModel(explicit, { ...router, baseUrl: 'https://private.example/v1' })).toMatchObject({ maxContextTokens: 16000, maxTokens: 1000 });
    });
    it('retains deliberate capability overrides without turning metadata into hosted features', () => {
        const model = resolveProviderModel({ name: 'moonshotai/kimi-k3', capabilityOverrides: { supportsTools: false } }, nvidia);
        expect(model.supportsTools).toBe(false);
        expect(model.supportsVision).toBe(true);
        expect(model.nativeCapabilities).toBeUndefined();
    });
    it('does not interpret availability-only discovery as capabilities', () => {
        expect(providerModelSnapshot(router, 'vendor/new', { id: 'vendor/new' })).toBeUndefined();
        expect(resolveProviderModel({ name: 'unknown/model', supportsTools: false }, nvidia).capabilitySource).toBeUndefined();
    });
    it('clamps explicit token limits to the endpoint without increasing a lower budget', () => {
        const info = { id: 'new/model', supportsTools: true, maxTokens: 8192, maxContextTokens: 32000 };
        expect(resolveProviderModel({ name: info.id, maxTokens: 100000, maxContextTokens: 100000 }, router, info)).toMatchObject({ maxTokens: 8192, maxContextTokens: 32000 });
        expect(resolveProviderModel({ name: info.id, maxTokens: 1000, maxContextTokens: 16000 }, router, info)).toMatchObject({ maxTokens: 1000, maxContextTokens: 16000 });
    });
});
