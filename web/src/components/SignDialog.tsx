/**
 * 提交签到面板（内联，不用原生对话框）。
 *
 * 定位签到的服务端会校验坐标是否落在考勤半径内，所以这里让用户填经纬度；
 * 浏览器支持时提供「用当前位置」按钮（Geolocation API）。
 */
import { useState } from 'react';

export interface SignTarget {
  detail_id: string;
  attendance_id: string;
  title: string;
  attendance_type: number;
  location_type: number | null;
  address_name: string;
  /** **平台要求**的坐标与半径（来自考勤接口，不是学生提交的那个）。
   *  有它就能直接按要求的点提交，不必人到现场。 */
  longitude?: string;
  latitude?: string;
  radius?: number | null;
}

interface Props {
  target: SignTarget;
  disabled?: boolean;
  /** 返回 `{ok:false}` 表示失败 —— 面板据此保持打开并就地显示原因。
   *  返回 undefined（旧调用方）按成功处理，保持向后兼容。 */
  onSubmit: (payload: {
    detail_id: string; attendance_id: string; type: number;
    longitude?: string; latitude?: string; address?: string; location_type?: number | null;
  }) => void | { ok?: boolean; error?: string } | Promise<void | { ok?: boolean; error?: string }>;
}

export default function SignDialog({ target, disabled, onSubmit }: Props) {
  const [open, setOpen] = useState(false);
  // 默认就填**平台要求**的坐标 —— 这样定位签到不需要人到现场，
  // 也不必手抄教室坐标。用户仍可点「用当前位置」改成真实位置。
  const [lon, setLon] = useState(target.longitude || '');
  const [lat, setLat] = useState(target.latitude || '');
  const [addr, setAddr] = useState(target.address_name || '');
  /** 用户是否改成了真实定位；改了就不再覆盖 */
  const [usedGeo, setUsedGeo] = useState(false);
  const [busy, setBusy] = useState(false);
  const [geoErr, setGeoErr] = useState('');
  /** 提交失败的原因 —— **显示在面板内**，不能只丢给页面顶部的提示条（它可能在视口外） */
  const [submitErr, setSubmitErr] = useState('');

  const needLoc = target.location_type === 1;

  /**
   * 把浏览器的 `GeolocationPositionError` 翻成人话。
   *
   * 原先直接把 `err.message` 透出去，用户看到的是
   * 「定位失败：User denied Geolocation」这种英文原文。
   */
  function geoMessage(err: GeolocationPositionError): string {
    switch (err.code) {
      case err.PERMISSION_DENIED:
        return '浏览器拒绝了定位权限。可在地址栏允许定位后重试，或手动填写教室坐标。';
      case err.POSITION_UNAVAILABLE:
        return '拿不到定位信息（可能是信号或系统定位服务未开启）。请手动填写坐标。';
      case err.TIMEOUT:
        return '定位超时。请重试，或手动填写教室坐标。';
      default:
        return `定位失败：${err.message}`;
    }
  }

  function useCurrentPosition() {
    setGeoErr('');
    if (!navigator.geolocation) { setGeoErr('浏览器不支持定位，请手动填写坐标。'); return; }
    navigator.geolocation.getCurrentPosition(
      (pos) => {
        setLon(pos.coords.longitude.toFixed(6));
        setLat(pos.coords.latitude.toFixed(6));
        setUsedGeo(true);
        setGeoErr(`已获取（精度约 ${Math.round(pos.coords.accuracy)} 米）`);
      },
      (err) => setGeoErr(geoMessage(err)),
      { enableHighAccuracy: true, timeout: 10000 },
    );
  }

  if (!open) {
    return (
      <button className="btn btn-sm btn-primary" disabled={disabled} onClick={() => setOpen(true)}>
        签到
      </button>
    );
  }

  const missingLoc = needLoc && (!lon || !lat);
  const tooFar = false;

  return (
    <div style={{
      display: 'inline-flex', flexDirection: 'column', gap: 6, padding: '10px 12px',
      border: '1px solid var(--border)', borderRadius: 'var(--radius-sm)',
      background: 'var(--bg)', minWidth: 280,
    }}>
      <strong style={{ fontSize: '0.85rem' }}>{target.title}</strong>
      {needLoc && (
        <>
          <span className="muted" style={{ fontSize: '0.78rem' }}>
            {usedGeo
              ? '已使用你的真实定位。'
              : (target.longitude && target.latitude)
                ? <>
                    已自动填入<b>平台要求的坐标</b>
                    {target.radius ? `（半径 ${target.radius} 米内有效）` : ''}。
                    通常可直接提交；若想用真实定位，点「用当前位置」。
                  </>
                : '此场次需要定位。可填教室坐标，或点「用当前位置」。'}
          </span>
          <div className="row" style={{ gap: 6 }}>
            <input className="input" style={{ fontSize: '0.8rem', width: 110 }}
              placeholder="经度" value={lon} onChange={(e) => setLon(e.target.value)} />
            <input className="input" style={{ fontSize: '0.8rem', width: 110 }}
              placeholder="纬度" value={lat} onChange={(e) => setLat(e.target.value)} />
            <button className="btn btn-sm" type="button" onClick={useCurrentPosition}>用当前位置</button>
          </div>
          <input className="input" style={{ fontSize: '0.8rem' }}
            placeholder="地址（可选）" value={addr} onChange={(e) => setAddr(e.target.value)} />
          {/* geoErr 同时承载「已获取」这种**成功**信息，
              所以不能一律用警告色 —— 那会把成功画成异常。 */}
          {geoErr && (
            <span className="muted" style={{ fontSize: '0.75rem', color: 'var(--ok)' }}>{geoErr}</span>
          )}
        </>
      )}
      {submitErr && (
        <div className="msg-bar msg-err" style={{ margin: 0, fontSize: '0.78rem' }}>
          签到失败：{submitErr}
        </div>
      )}
      <div className="row" style={{ gap: 6 }}>
        <button className="btn btn-sm btn-primary"
          disabled={busy || missingLoc}
          onClick={async () => {
            setBusy(true);
            setSubmitErr('');
            const r = await onSubmit({
              detail_id: target.detail_id,
              attendance_id: target.attendance_id,
              type: target.attendance_type,
              longitude: lon || undefined,
              latitude: lat || undefined,
              address: addr || undefined,
              location_type: target.location_type,
            });
            setBusy(false);
            // ⚠️ 只有成功才关面板。原先写在 finally 里无条件关闭，
            // 失败时用户只看到面板消失，会合理推断成「提交成功了」。
            if (r?.ok === false) {
              setSubmitErr(r.error || '未知原因');
              return;
            }
            setOpen(false);
          }}>
          {busy ? '提交中…' : '确认签到'}
        </button>
        <button className="btn btn-sm" disabled={busy} onClick={() => setOpen(false)}>取消</button>
      </div>
    </div>
  );
}
