/// <reference types="astro/client" />

declare namespace App {
  interface Locals {
    user: import('./lib/auth').SessionUser | null;
    /**
     * 后端不可用时的原因（`getSession` 返回 `unavailable`）。
     *
     * 与 `user === null` 是两回事：前者是「后端挂了，登录态无从判断」，
     * 后者是「确实没登录」。混为一谈会把用户误弹到登录页。
     */
    backendError?: string;
  }
}

// ---- korikura-webui ----
//
// 这个包直接发源码（.astro / .tsx），**没有类型声明文件**。
// 不加这几行的话 `astro check` 会报一片 `Cannot find module`，
// 把真正的类型错误淹掉 —— 那正是它该发挥作用的时候。
declare module 'korikura-webui/components/*';
declare module 'korikura-webui/lib/*';
declare module 'korikura-webui/styles.css';
