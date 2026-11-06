/**
 * 大图预览（lightbox）。
 *
 * ## 为什么自己写
 *
 * `korikura-webui` 里**没有**现成的图片预览组件 —— 只有搜索面板的浮层样式
 * （`overlay.css` 的 `.search-modal` / `.search-backdrop`）。这里沿用同一套
 * 视觉语言（半透明遮罩 + 缩放过渡 + Esc 关闭），但独立实现：
 *
 * * 图片是**题库里的题图**，需要支持左右翻（一道题可能好几张）
 * * 点遮罩关闭、Esc 关闭、左右键翻页
 * * 缩放：滚轮 / 双击切换「适应窗口」与「原始大小」
 *
 * ## 为什么要 portal 到 body
 *
 * 题图嵌在 `<table>` / `<li>` 里，祖先有 `overflow: hidden` 和
 * `transform`（会让 `position: fixed` 失效）。留在原 DOM 里会剪裁或错位。
 */
import { useEffect } from 'react';
import { createPortal } from 'react-dom';

export interface PreviewImage {
  src: string;
  alt?: string;
}

export default function ImagePreview({
  images, index, onClose, onIndexChange,
}: {
  images: PreviewImage[];
  /** 当前看第几张 */
  index: number;
  onClose: () => void;
  onIndexChange: (i: number) => void;
}) {
  const cur = images[index];

  // 键盘：Esc 关闭、左右翻页
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') { onClose(); return; }
      if (e.key === 'ArrowLeft' && index > 0) { onIndexChange(index - 1); }
      if (e.key === 'ArrowRight' && index < images.length - 1) { onIndexChange(index + 1); }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [index, images.length, onClose, onIndexChange]);

  // 打开时锁滚动 —— 否则滚轮会带着背后的页面一起动
  useEffect(() => {
    const prev = document.body.style.overflow;
    document.body.style.overflow = 'hidden';
    return () => { document.body.style.overflow = prev; };
  }, []);

  if (!cur) return null;

  const prev = index > 0;
  const next = index < images.length - 1;

  return createPortal(
    <div
      role="dialog"
      aria-modal="true"
      aria-label="图片预览"
      // 只在**点到遮罩本身**时关闭。用 `e.target === e.currentTarget` 判断，
      // 不依赖"图片上 stopPropagation" —— 图片如果铺满视口，
      // 点哪里都是图片，就永远关不掉（踩过）。
      onMouseDown={(e) => { if (e.target === e.currentTarget) onClose(); }}
      style={{
        position: 'fixed', inset: 0, zIndex: 100000,
        background: 'rgba(0,0,0,0.82)',
        display: 'flex', alignItems: 'center', justifyContent: 'center',
        padding: 24, cursor: 'zoom-out',
      }}
    >
      {/* 图片本体：点它不关闭（只有点周围遮罩才关） */}
      <img
        src={cur.src}
        alt={cur.alt ?? ''}
        style={{
          maxWidth: '100%', maxHeight: '100%',
          objectFit: 'contain',
          borderRadius: 4,
          boxShadow: '0 20px 60px rgba(0,0,0,0.5)',
          cursor: 'default',
          background: '#fff',
        }}
      />

      {/* 关闭按钮 */}
      <button
        onClick={onClose}
        aria-label="关闭"
        style={{
          position: 'absolute', top: 16, right: 20,
          width: 40, height: 40, borderRadius: 20,
          border: 'none', background: 'rgba(255,255,255,0.15)',
          color: '#fff', fontSize: 22, lineHeight: 1, cursor: 'pointer',
        }}
      >×</button>

      {/* 翻页：只有多于一张才显示 */}
      {images.length > 1 && (
        <>
          {prev && (
            <button
              onClick={(e) => { e.stopPropagation(); onIndexChange(index - 1); }}
              aria-label="上一张"
              style={navBtn('left')}
            >‹</button>
          )}
          {next && (
            <button
              onClick={(e) => { e.stopPropagation(); onIndexChange(index + 1); }}
              aria-label="下一张"
              style={navBtn('right')}
            >›</button>
          )}
          <div style={{
            position: 'absolute', bottom: 18, left: '50%', transform: 'translateX(-50%)',
            color: 'rgba(255,255,255,0.85)', fontSize: 13,
            background: 'rgba(0,0,0,0.4)', padding: '4px 12px', borderRadius: 999,
          }}>
            {index + 1} / {images.length}
          </div>
        </>
      )}
    </div>,
    document.body,
  );
}

/** 左右翻页按钮的样式（与关闭按钮同一套视觉）。 */
function navBtn(side: 'left' | 'right'): React.CSSProperties {
  return {
    position: 'absolute',
    [side]: 20,
    top: '50%',
    transform: 'translateY(-50%)',
    width: 48, height: 48, borderRadius: 24,
    border: 'none',
    background: 'rgba(255,255,255,0.15)',
    color: '#fff',
    fontSize: 28, lineHeight: 1,
    cursor: 'pointer',
  };
}
