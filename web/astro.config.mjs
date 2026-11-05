// @ts-check
import { defineConfig } from 'astro/config';
import react from '@astrojs/react';
import node from '@astrojs/node';
import tailwindcss from '@tailwindcss/vite';

/** 后端地址。浏览器只访问前端同源 /api/*，由 Astro 服务端转发。 */
const BACKEND = process.env.NEUMOOC_BACKEND ?? 'http://127.0.0.1:8788';
const HOST = process.env.HOST ?? '127.0.0.1';

export default defineConfig({
  // 站点域名：真实值来自环境变量（部署在 systemd 里），仓库里只留占位。
  // 它影响 canonical / sitemap / 绝对链接的生成。
  site: process.env.NEUMOOC_SITE_URL || 'https://example.com',
  trailingSlash: 'never',
  compressHTML: true,
  build: { assets: 'assets', inlineStylesheets: 'never' },
  // SSR：需要服务端代理 /api/*，不能用静态输出
  output: 'server',
  adapter: node({ mode: 'standalone' }),
  // Astro 默认 checkOrigin 会拦掉经反代的 POST（报 "Cross-site POST form
  // submissions are forbidden"），因为反代后 Host 与 Origin 对不上。
  // 关掉它，改用自建 Host 白名单（见 src/middleware.ts）—— 更可控。
  security: { checkOrigin: false },

  integrations: [react()],
  vite: {
    plugins: [tailwindcss()],
    resolve: { alias: { '@': '/src' } },
    server: {
      host: HOST,
      proxy: {
        // 开发态同样走代理，保证 dev 与 prod 行为一致
        '/api': { target: BACKEND, changeOrigin: true, ws: false },
      },
    },
  },
});
