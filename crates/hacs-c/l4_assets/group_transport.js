const API = "";              // 同源托管：直接打本站 /api
async function api(method, path, body) {
  const opt = { method, credentials: "include", headers: {} };
  if (body !== undefined) { opt.headers["Content-Type"] = "application/json"; opt.body = JSON.stringify(body); }
  const r = await fetch(API + path, opt);
  const txt = await r.text();
  let data = null; try { data = txt ? JSON.parse(txt) : null; } catch (e) { data = txt; }
  if (r.status === 401) {
    // 未登录：本服务不自己发登录页 —— 转交门户登录页，并把「回本页」带上
    const back = encodeURIComponent(location.pathname + location.search);
    location.href = "/login?next=" + back;
    throw new Error("需要登录，正在跳转登录页…");
  }
  if (!r.ok) {
    const code = (data && data.error && (data.error.code || data.error)) || ("HTTP " + r.status);
    const hint = {
      mention_required: "Agent 发言必须 @（人不需要）",
      unknown_mention: "提到了不在群里的人",
      unknown_member: "有成员不存在（看名册）",
      only_owner_can_close: "只有发起人能宣布散会",
      session_archived: "这个群已经散会（归档）了",
      creator_required: "建群必须由人发起",
      bad_json: "请求体不是合法 JSON"
    }[code] || "";
    throw new Error(code + (hint ? " — " + hint : ""));
  }
  return data;
}
const TGROUP_DEFAULT = "meet_20260921_demo01";
