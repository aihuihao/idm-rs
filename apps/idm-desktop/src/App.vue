<template>
  <div class="app" @click="closeContextMenu">
    <h1>idm-rs 下载管理器</h1>

    <div v-if="readOnly" class="banner">
      后台运行器正在运行，界面为只读。请先执行 idm-cli runner stop 后重启本程序。
    </div>

    <div v-if="fatalError" class="error-banner">
      <span>{{ fatalError }}</span>
      <button class="error-close" @click.stop="dismissFatalError">×</button>
    </div>

    <div v-if="browserBannerVisible" class="banner browser-banner">
      <span>{{ browserBannerText }}</span>
      <button v-if="browserBannerNeedsExtension" class="browser-action" @click="onInstallFirefoxFromBanner">安装</button>
      <button v-else class="browser-action" @click="onEnableBrowserFromBanner">启用</button>
      <button class="browser-action" @click="onDismissBrowserBanner">不再提示</button>
    </div>

    <div class="toolbar">
      <button :disabled="readOnly" @click="openAddDialog">
        <svg class="ico" viewBox="0 0 16 16"><path :d="icons.add" /></svg>添加任务
      </button>
      <button :disabled="readOnly || !canResumeSelection" @click="onResumeSelected">
        <svg class="ico" viewBox="0 0 16 16"><path :d="icons.resume" /></svg>继续
      </button>
      <button :disabled="readOnly || !canPauseSelection" @click="onPauseSelected">
        <svg class="ico" viewBox="0 0 16 16"><path :d="icons.pause" /></svg>暂停
      </button>
      <button :disabled="readOnly || selectedIds.size === 0" @click="askDeleteSelected">
        <svg class="ico" viewBox="0 0 16 16"><path :d="icons.delete" /></svg>删除
      </button>
      <button :disabled="readOnly" @click="confirmClearCompleted = true">
        <svg class="ico" viewBox="0 0 16 16"><path :d="icons.clear" /></svg>清除已完成
      </button>
      <button @click="openOptions">
        <svg class="ico" viewBox="0 0 16 16"><path :d="icons.options" /></svg>选项
      </button>
    </div>

    <div class="main">
      <aside class="sidebar">
        <div class="side-title">状态</div>
        <button
          v-for="s in statusCats"
          :key="s.key"
          class="side-item"
          :class="{ active: filterStatus === s.key }"
          @click="filterStatus = s.key"
        >
          <span>{{ s.label }}</span><span class="count">{{ statusCount(s.key) }}</span>
        </button>
        <div class="side-title kind-title">类型</div>
        <button
          v-for="k in kindCats"
          :key="k.key"
          class="side-item"
          :class="{ active: filterKind === k.key }"
          @click="filterKind = k.key"
        >
          <svg class="ico" viewBox="0 0 16 16"><path :d="icons[k.icon]" /></svg>
          <span>{{ k.label }}</span><span class="count">{{ kindCount(k.key) }}</span>
        </button>
      </aside>

      <div class="content" ref="contentEl">
        <div class="table-wrap" ref="tableWrapEl">
        <table class="tasks">
          <colgroup>
            <col class="col-icon" />
            <col class="col-name" />
            <col class="col-size" />
            <col class="col-progress" />
            <col class="col-speed" />
            <col class="col-eta" />
            <col class="col-state" />
          </colgroup>
          <thead>
            <tr>
              <th></th>
              <th>文件名</th>
              <th class="num">大小</th>
              <th class="num">进度</th>
              <th class="num">速度</th>
              <th class="num">剩余时间</th>
              <th>状态</th>
            </tr>
          </thead>
          <tbody>
            <tr
              v-for="task in filteredTasks"
              :key="task.id"
              :class="{ selected: selectedIds.has(task.id) }"
              @click="onRowClick($event, task)"
              @dblclick="onRowDblClick(task)"
              @contextmenu.prevent="onContextMenu($event, task)"
            >
              <td class="type-icon">
                <svg class="ico" viewBox="0 0 16 16" :title="kindLabel(task.kind)">
                  <path :d="icons[kindIcon(task.kind)]" />
                </svg>
              </td>
              <td class="name" :title="task.file_name || task.url_display">
                {{ task.file_name || task.url_display }}
              </td>
              <td class="num">{{ formatSize(task.total_size) }}</td>
              <td class="num">
                <div v-if="task.total_size && task.total_size > 0" class="pbar">
                  <div class="pbar-fill" :class="progressClass(task)" :style="{ width: percent(task) + '%' }"></div>
                  <span class="pbar-text">{{ percent(task) }}%</span>
                </div>
                <span v-else>—</span>
              </td>
              <td class="num">{{ formatSpeed(task.speed_bps) }}</td>
              <td class="num">{{ formatEta(task) }}</td>
              <td :title="task.state === 'Failed' ? (task.last_error || '') : ''">{{ stateText(task) }}</td>
            </tr>
            <tr v-if="filteredTasks.length === 0">
              <td colspan="7" class="empty">暂无任务</td>
            </tr>
          </tbody>
        </table>
        </div>

        <div
          v-if="selectedIds.size === 1 && !detailCollapsed"
          class="splitter"
          @mousedown="startDrag($event)"
        ></div>

        <section
          v-if="selectedIds.size === 1"
          ref="detailPanelEl"
          class="detail"
          :class="{ collapsed: detailCollapsed }"
          :style="detailHeightStyle"
        >
          <div class="detail-head" ref="detailHeadEl" @click="detailCollapsed = !detailCollapsed">
            <span>任务详情 — {{ singleSelected?.file_name || singleSelected?.url_display }}</span>
            <button class="fold-btn" @click.stop="detailCollapsed = !detailCollapsed">
              {{ detailCollapsed ? '展开' : '折叠' }}
            </button>
          </div>
          <div v-if="!detailCollapsed" ref="detailBodyEl" class="detail-body">
            <template v-if="detail && detail.total_size != null && detail.chunks.length > 0">
              <div class="detail-meta">
                分段 {{ detail.segments }}　下载中 {{ detail.downloading }}　排队 {{ detail.queued }}　最多同时 {{ detail.max_concurrent }}
              </div>
              <div class="segbar">
                <div
                  v-for="c in detail.chunks"
                  :key="c.index"
                  class="seg"
                  :style="{ left: segLeft(c) + '%', width: segWidth(c) + '%' }"
                >
                  <div class="seg-fill" :class="chunkClass(c)" :style="{ width: chunkPct(c) + '%' }"></div>
                </div>
              </div>
              <div class="chunk-table-wrap" ref="chunkTableWrapEl">
                <table class="chunks">
                  <thead>
                    <tr><th>编号</th><th>范围</th><th class="num">已下载/总量</th><th class="num">速度</th><th>状态</th></tr>
                  </thead>
                  <tbody>
                    <tr v-for="c in detail.chunks" :key="c.index">
                      <td>{{ c.index }}</td>
                      <td class="range">{{ formatBytes(c.start) }} - {{ formatBytes(c.end + 1) }}</td>
                      <td class="num">{{ formatBytes(c.downloaded) }}/{{ formatBytes(c.end + 1 - c.start) }}</td>
                      <td class="num">{{ chunkSpeedText(c) }}</td>
                      <td>{{ chunkStateText(c.state) }}</td>
                    </tr>
                  </tbody>
                </table>
              </div>
            </template>
            <div v-else class="detail-empty">该任务为单连接下载，无分段信息</div>
          </div>
        </section>
      </div>
    </div>

    <div class="statusbar">
      <span>下载中：{{ downloadingCount }}</span>
      <span>总速度：{{ formatSpeed(totalSpeed) }}</span>
      <span>全局限速：{{ globalSpeed === 0 ? '不限速' : formatSpeed(globalSpeed) }}</span>
    </div>

    <!-- 右键菜单 -->
    <div v-if="contextMenu.visible" class="ctx-menu" :style="{ left: contextMenu.x + 'px', top: contextMenu.y + 'px' }">
      <button :disabled="readOnly || !ctxCanResume" @click.stop="ctxAction('resume')">继续</button>
      <button :disabled="readOnly || !ctxCanPause" @click.stop="ctxAction('pause')">暂停</button>
      <button :disabled="readOnly" @click.stop="ctxAction('delete')">删除</button>
      <button :disabled="readOnly || ctxTask?.state !== 'Completed'" @click.stop="ctxAction('reveal')">打开文件夹</button>
    </div>

    <!-- 添加任务对话框 -->
    <div v-if="addOpen" class="overlay" @click.self="addOpen = false">
      <div class="dialog">
        <h2>添加任务</h2>
        <input
          v-model="addUrl"
          type="text"
          placeholder="输入下载网址"
          autofocus
          @keyup.enter="onAdd"
        />
        <div v-if="addError" class="error-text">{{ addError }}</div>
        <div class="dialog-ops">
          <button @click="addOpen = false">取消</button>
          <button :disabled="!addUrl.trim()" @click="onAdd">开始下载</button>
        </div>
      </div>
    </div>

    <!-- 删除确认框 -->
    <div v-if="confirmDelete" class="overlay" @click.self="confirmDelete = false">
      <div class="dialog">
        <p>{{ deleteConfirmText }}</p>
        <label class="checkbox">
          <input v-model="deleteFile" type="checkbox" />
          同时删除已下载的文件
        </label>
        <div v-if="confirmError" class="error-text">{{ confirmError }}</div>
        <div class="dialog-ops">
          <button @click="confirmDelete = false">取消</button>
          <button :disabled="readOnly" @click="onConfirmDelete">确定</button>
        </div>
      </div>
    </div>

    <!-- 清除已完成确认框 -->
    <div v-if="confirmClearCompleted" class="overlay" @click.self="confirmClearCompleted = false">
      <div class="dialog">
        <p>确定清除所有已完成的任务记录吗？已下载的文件不会被删除。</p>
        <div v-if="clearError" class="error-text">{{ clearError }}</div>
        <div class="dialog-ops">
          <button @click="confirmClearCompleted = false">取消</button>
          <button :disabled="readOnly" @click="onClearCompleted">确定</button>
        </div>
      </div>
    </div>

    <!-- 选项对话框 -->
    <div v-if="optionsOpen" class="overlay" @click.self="closeOptions">
      <div class="dialog options-dialog">
        <h2>选项</h2>
        <div class="field">
          <label>下载目录</label>
          <div class="dir-row">
            <input v-model="draftSettings.download_dir" :disabled="readOnly" type="text" />
            <button :disabled="readOnly" @click="onPickDir">选择…</button>
          </div>
        </div>
        <div class="field">
          <label>全局限速（KB/秒，0 表示不限速）</label>
          <input v-model.number="draftSpeedKb" :disabled="readOnly" type="number" min="0" />
        </div>
        <div class="field">
          <label>同时下载任务数（1~16）</label>
          <input v-model.number="draftSettings.max_tasks" :disabled="readOnly" type="number" min="1" max="16" />
        </div>
        <div class="field">
          <label>每任务默认连接数（1~32）</label>
          <input v-model.number="draftSettings.default_connections" :disabled="readOnly" type="number" min="1" max="32" />
          <label class="checkbox">
            <input v-model="draftSettings.resume_uses_current_connections" :disabled="readOnly" type="checkbox" />
            继续下载时使用当前的默认连接数
          </label>
          <span class="hint">勾选后，暂停的任务继续时改用上面的连接数；已下载的分段保留</span>
        </div>
        <div class="field">
          <label>每台服务器最大连接数（1~32）</label>
          <input v-model.number="draftSettings.max_connections_per_host" :disabled="readOnly" type="number" min="1" max="32" />
          <span class="hint">保存后立即对正在下载的任务生效</span>
        </div>
        <div class="field browser-field">
          <label>浏览器集成</label>
          <div class="browser-status-row">
            <span>当前状态：{{ browserStatusLabel }}</span>
            <button v-if="browserStatus !== 'Ok'" @click="onEnableBrowser">启用<span v-if="browserStatus === 'Broken'">/修复</span></button>
            <button v-if="browserStatus === 'Ok'" @click="onDisableBrowser">移除</button>
          </div>
          <span class="hint">启用后，Firefox 安装 idm-rs 扩展即可自动接管下载</span>
          <div class="browser-status-row">
            <span>Firefox 扩展：{{ firefoxStatusLabel }}</span>
            <button v-if="firefoxInstallable" @click="onInstallFirefox">安装 Firefox 扩展</button>
          </div>
          <span v-if="firefoxActionMessage" class="hint">{{ firefoxActionMessage }}</span>
          <span v-if="browserActionError" class="error-text">{{ browserActionError }}</span>
        </div>
        <div class="field">
          <label class="checkbox">
            <input v-model="draftSettings.autostart" :disabled="readOnly" type="checkbox" />
            开机自动启动（以托盘方式启动）
          </label>
        </div>
        <div class="field">
          <label class="checkbox">
            <input v-model="draftSettings.notify_on_complete" :disabled="readOnly" type="checkbox" />
            下载完成时通知
          </label>
        </div>
        <div v-if="optionsError" class="error-text">{{ optionsError }}</div>
        <div class="dialog-ops">
          <button @click="closeOptions">取消</button>
          <button :disabled="readOnly" @click="onSaveOptions">保存</button>
        </div>
      </div>
    </div>
  </div>
</template>

<script setup lang="ts">
import { computed, nextTick, onMounted, onUnmounted, ref, watch } from 'vue';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';

interface TaskView {
  id: number;
  kind: string;
  file_name: string | null;
  url_display: string;
  total_size: number | null;
  downloaded: number;
  speed_bps: number;
  state: string;
  pause_reason: string | null;
  last_error: string | null;
}

interface Settings {
  download_dir: string;
  global_max_speed: number;
  max_tasks: number;
  default_connections: number;
  max_connections_per_host: number;
  resume_uses_current_connections: boolean;
  autostart: boolean;
  notify_on_complete: boolean;
  browser_integration_dismissed: boolean;
}

interface ChunkView {
  index: number;
  start: number;
  end: number;
  downloaded: number;
  speed_bps: number;
  state: string;
}

interface TaskDetail {
  id: number;
  total_size: number | null;
  connections: number;
  segments: number;
  downloading: number;
  queued: number;
  max_concurrent: number;
  chunks: ChunkView[];
}

// 自己绘制的简单图形路径（16x16）。
const icons: Record<string, string> = {
  add: 'M8 3v10M3 8h10',
  resume: 'M4 3l9 5-9 5z',
  pause: 'M5 3v10M11 3v10',
  delete: 'M3 5h10M6 5V3h4v2M5 5l1 8h4l1-8',
  clear: 'M3 8l3 3 7-7',
  options: 'M8 5a3 3 0 1 0 0 6 3 3 0 0 0 0-6M8 1.5v2M8 12.5v2M1.5 8h2M12.5 8h2',
  archive: 'M2 4h12v3H2zM3 7v7h10V7M6 10h4',
  video: 'M2 4h12v8H2zM6 6l4 2-4 2z',
  audio: 'M6 12V5l6-1v7M6 12a2 2 0 1 1-2-2M12 11a2 2 0 1 1-2-2',
  document: 'M4 2h6l3 3v9H4zM10 2v3h3',
  program: 'M5 5h6v6H5zM3 8h2M11 8h2M8 3v2M8 11v2',
  other: 'M5 5h6v6H5z',
};

const statusCats = [
  { key: 'all', label: '全部' },
  { key: 'downloading', label: '下载中' },
  { key: 'incomplete', label: '未完成' },
  { key: 'completed', label: '已完成' },
];
const kindCats = [
  { key: 'archive', label: '压缩包', icon: 'archive' },
  { key: 'video', label: '视频', icon: 'video' },
  { key: 'audio', label: '音频', icon: 'audio' },
  { key: 'document', label: '文档', icon: 'document' },
  { key: 'program', label: '程序', icon: 'program' },
  { key: 'other', label: '其他', icon: 'other' },
];

function kindIcon(kind: string): string {
  return icons[kind] ? kind : 'other';
}
function kindLabel(kind: string): string {
  const found = kindCats.find((k) => k.key === kind);
  return found ? found.label : '其他';
}

const readOnly = ref(false);
const tasks = ref<TaskView[]>([]);
const selectedIds = ref<Set<number>>(new Set());
const filterStatus = ref('all');
const filterKind = ref('all');
const fatalError = ref('');

type BrowserStatus = 'NotInstalled' | 'Ok' | 'Broken';
type FirefoxStatus = 'firefoxMissing' | 'notInstalled' | 'unknown' | { installed: string[] };
const browserStatus = ref<BrowserStatus>('NotInstalled');
const browserBannerVisible = ref(false);
const browserActionError = ref('');
const firefoxStatus = ref<FirefoxStatus>('unknown');
const firefoxActionMessage = ref('');
const browserStatusLabel = computed(() => {
  if (browserStatus.value === 'Ok') return '已启用';
  if (browserStatus.value === 'Broken') return '需要修复';
  return '未启用';
});
const firefoxStatusLabel = computed(() => {
  const s = firefoxStatus.value;
  if (typeof s === 'object' && 'installed' in s) return `已安装（${s.installed.join('、')}）`;
  if (s === 'firefoxMissing') return '未检测到 Firefox';
  if (s === 'notInstalled') return '未安装';
  return '无法判断';
});
const firefoxInstallable = computed(() => {
  const s = firefoxStatus.value;
  return s === 'notInstalled' || s === 'firefoxMissing';
});
const extensionMissing = computed(() => {
  const s = firefoxStatus.value;
  return s === 'notInstalled' || s === 'firefoxMissing';
});
const browserBannerText = computed(() =>
  browserStatus.value === 'NotInstalled'
    ? '浏览器集成未启用，Firefox 下载不会被接管'
    : 'Firefox 扩展未安装'
);
const browserBannerNeedsExtension = computed(
  () => browserStatus.value !== 'NotInstalled' && extensionMissing.value
);

const addOpen = ref(false);
const addUrl = ref('');
const addError = ref('');

const confirmDelete = ref(false);
const deleteFile = ref(false);
const confirmError = ref('');
const confirmClearCompleted = ref(false);
const clearError = ref('');

const optionsOpen = ref(false);
const draftSettings = ref<Settings>({
  download_dir: '', global_max_speed: 0, max_tasks: 3,
  default_connections: 8, max_connections_per_host: 8, resume_uses_current_connections: true,
  autostart: false, notify_on_complete: true, browser_integration_dismissed: false,
});
const draftSpeedKb = ref(0);
const optionsError = ref('');

const globalSpeed = ref(0);

const contextMenu = ref({ visible: false, x: 0, y: 0, taskId: 0 });

const detail = ref<TaskDetail | null>(null);
const detailCollapsed = ref(false);

const contentEl = ref<HTMLElement | null>(null);
const detailPanelEl = ref<HTMLElement | null>(null);
const detailHeadEl = ref<HTMLElement | null>(null);
const detailBodyEl = ref<HTMLElement | null>(null);
const chunkTableWrapEl = ref<HTMLElement | null>(null);
const detailHeightStyle = ref<Record<string, string>>({});
const MIN_DETAIL_HEIGHT = 120;
const HEADER_PLUS_FIVE_ROWS = 6 * 28; // 表头 + 5 行数据，约 168px

let unlisten: (() => void) | null = null;
let fatalTimer: ReturnType<typeof setTimeout> | null = null;
let detailTimer: ReturnType<typeof setInterval> | null = null;

// 用户拖动后的固定高度（null 表示按内容自动计算）。
let pinnedDetailHeight: number | null = null;

// 每个分块的最近采样 { t, downloaded }，键为 "index:start"。
interface ChunkSample { t: number; downloaded: number }
const chunkSamples = new Map<string, ChunkSample[]>();
const chunkSpeeds = new Map<string, number>();

const filteredTasks = computed(() =>
  tasks.value.filter((t) => statusMatch(t) && (filterKind.value === 'all' || t.kind === filterKind.value)),
);
const selectedTasks = computed(() => tasks.value.filter((t) => selectedIds.value.has(t.id)));
const singleSelected = computed(() => (selectedIds.value.size === 1 ? selectedTasks.value[0] ?? null : null));
const canResumeSelection = computed(
  () => selectedTasks.value.length > 0 && selectedTasks.value.some((t) => t.state === 'Paused' || t.state === 'Failed'),
);
const canPauseSelection = computed(
  () => selectedTasks.value.length > 0 && selectedTasks.value.some((t) => t.state === 'Queued' || t.state === 'Downloading'),
);
const downloadingCount = computed(() => tasks.value.filter((t) => t.state === 'Downloading').length);
const totalSpeed = computed(() => tasks.value.reduce((sum, t) => sum + (t.speed_bps || 0), 0));
const deleteConfirmText = computed(() => {
  const n = selectedIds.value.size;
  return n > 1 ? `确定删除选中的 ${n} 个任务吗？` : '确定删除任务吗？';
});
const ctxTask = computed(() => tasks.value.find((t) => t.id === contextMenu.value.taskId) ?? null);
const ctxCanResume = computed(() => ctxTask.value?.state === 'Paused' || ctxTask.value?.state === 'Failed');
const ctxCanPause = computed(() => ctxTask.value?.state === 'Queued' || ctxTask.value?.state === 'Downloading');

function statusMatch(t: TaskView): boolean {
  switch (filterStatus.value) {
    case 'downloading': return t.state === 'Downloading';
    case 'incomplete': return t.state === 'Queued' || t.state === 'Paused' || t.state === 'Failed';
    case 'completed': return t.state === 'Completed';
    default: return true;
  }
}
function statusCount(key: string): number {
  return tasks.value.filter((t) => (key === 'all' ? true : statusMatchKey(t, key))).length;
}
function statusMatchKey(t: TaskView, key: string): boolean {
  switch (key) {
    case 'downloading': return t.state === 'Downloading';
    case 'incomplete': return t.state === 'Queued' || t.state === 'Paused' || t.state === 'Failed';
    case 'completed': return t.state === 'Completed';
    default: return true;
  }
}
function kindCount(key: string): number {
  return key === 'all' ? tasks.value.length : tasks.value.filter((t) => t.kind === key).length;
}

function dismissFatalError(): void {
  if (fatalTimer !== null) { clearTimeout(fatalTimer); fatalTimer = null; }
  fatalError.value = '';
}
function showFatalError(message: string): void {
  dismissFatalError();
  fatalError.value = message;
  fatalTimer = setTimeout(() => { fatalTimer = null; fatalError.value = ''; }, 8000);
}
function errText(err: unknown): string {
  if (typeof err === 'string') return err;
  if (err instanceof Error) return err.message;
  return String(err);
}
function isStaleTaskError(message: string): boolean {
  return message.includes('neither active nor queued') || message.includes('不在活动或排队');
}

function formatSize(bytes: number | null | undefined): string {
  if (bytes === null || bytes === undefined) return '未知';
  const units = ['B', 'KB', 'MB', 'GB'];
  let value = bytes;
  let index = 0;
  while (value >= 1024 && index < units.length - 1) { value /= 1024; index += 1; }
  return `${value.toFixed(1)} ${units[index]}`;
}
function formatBytes(bytes: number): string {
  if (bytes === 0) return '0 B';
  if (bytes < 0) return '0 B';
  const units = ['B', 'KB', 'MB', 'GB'];
  let value = bytes;
  let index = 0;
  while (value >= 1024 && index < units.length - 1) { value /= 1024; index += 1; }
  return `${value.toFixed(1)} ${units[index]}`;
}
function formatSpeed(bytesPerSecond: number): string {
  if (!bytesPerSecond || bytesPerSecond <= 0) return '—';
  return `${formatSize(bytesPerSecond)}/秒`;
}
function percent(task: TaskView): number {
  if (!task.total_size || task.total_size <= 0) return 0;
  const value = Math.round((task.downloaded / task.total_size) * 1000) / 10;
  return Math.max(0, Math.min(100, value));
}
function formatEta(task: TaskView): string {
  if (!task.total_size || !task.speed_bps || task.speed_bps <= 0) return '—';
  const remaining = task.total_size - task.downloaded;
  if (remaining <= 0) return '—';
  const totalSeconds = Math.ceil(remaining / task.speed_bps);
  const hours = Math.floor(totalSeconds / 3600);
  const minutes = Math.floor((totalSeconds % 3600) / 60);
  const seconds = totalSeconds % 60;
  return `${hours}:${String(minutes).padStart(2, '0')}:${String(seconds).padStart(2, '0')}`;
}
function stateText(task: TaskView): string {
  switch (task.state) {
    case 'Queued': return '排队中';
    case 'Downloading': return '下载中';
    case 'Paused': return task.pause_reason === 'Interrupted' ? '已中断' : '已暂停';
    case 'Completed': return '已完成';
    case 'Failed': return '失败';
    default: return task.state;
  }
}
function progressClass(task: TaskView): string {
  switch (task.state) {
    case 'Completed': return 'done';
    case 'Failed': return 'failed';
    case 'Paused': return 'paused';
    default: return 'active';
  }
}

function setSelected(next: Set<number>): void {
  selectedIds.value = next;
}
function onRowClick(event: MouseEvent, task: TaskView): void {
  const next = new Set(selectedIds.value);
  if (event.ctrlKey || event.metaKey) {
    if (next.has(task.id)) next.delete(task.id); else next.add(task.id);
  } else {
    next.clear();
    next.add(task.id);
  }
  setSelected(next);
}
function onRowDblClick(task: TaskView): void {
  if (task.state !== 'Completed') return;
  void invoke('reveal_task', { id: task.id }).catch((err) => showFatalError(errText(err)));
}

function onContextMenu(event: MouseEvent, task: TaskView): void {
  if (!selectedIds.value.has(task.id)) setSelected(new Set([task.id]));
  contextMenu.value = { visible: true, x: event.clientX, y: event.clientY, taskId: task.id };
}
function closeContextMenu(): void {
  if (contextMenu.value.visible) contextMenu.value = { ...contextMenu.value, visible: false };
}
async function ctxAction(action: string): Promise<void> {
  const id = contextMenu.value.taskId;
  closeContextMenu();
  if (action === 'reveal') {
    await invoke('reveal_task', { id }).catch((err) => showFatalError(errText(err)));
    return;
  }
  await runOnIds([id], action);
}

async function runOnIds(ids: number[], action: string): Promise<void> {
  for (const id of ids) {
    try {
      if (action === 'pause') await invoke('pause_task', { id });
      else if (action === 'resume') await invoke('resume_task', { id });
      else if (action === 'delete') await invoke('remove_task', { id, deleteFile: deleteFile.value });
    } catch (err) {
      const message = errText(err);
      if (action === 'delete' || !isStaleTaskError(message)) {
        if (action === 'delete') { confirmError.value = message; }
        else { showFatalError(message); }
      }
      // 任务已结束等过期错误静默忽略
    }
  }
  await refreshTasks();
}

async function onPauseSelected(): Promise<void> {
  const ids = selectedTasks.value.filter((t) => t.state === 'Queued' || t.state === 'Downloading').map((t) => t.id);
  await runOnIds(ids, 'pause');
}
async function onResumeSelected(): Promise<void> {
  const ids = selectedTasks.value.filter((t) => t.state === 'Paused' || t.state === 'Failed').map((t) => t.id);
  await runOnIds(ids, 'resume');
}
function askDeleteSelected(): void {
  deleteFile.value = false;
  confirmError.value = '';
  confirmDelete.value = true;
}
async function onConfirmDelete(): Promise<void> {
  confirmError.value = '';
  const ids = Array.from(selectedIds.value);
  await runOnIds(ids, 'delete');
  confirmDelete.value = false;
}
async function onClearCompleted(): Promise<void> {
  clearError.value = '';
  try {
    await invoke('clear_completed');
    confirmClearCompleted.value = false;
    await refreshTasks();
  } catch (err) {
    clearError.value = errText(err);
  }
}

function openAddDialog(): void {
  addUrl.value = '';
  addError.value = '';
  addOpen.value = true;
}
async function onAdd(): Promise<void> {
  addError.value = '';
  const url = addUrl.value.trim();
  if (!url) return;
  try {
    await invoke('add_task', { url, connections: null });
    addOpen.value = false;
    await refreshTasks();
  } catch (err) {
    addError.value = errText(err);
  }
}

async function refreshTasks(): Promise<void> {
  try {
    tasks.value = await invoke<TaskView[]>('list_tasks');
    const valid = new Set(tasks.value.map((t) => t.id));
    const next = new Set(Array.from(selectedIds.value).filter((id) => valid.has(id)));
    if (next.size !== selectedIds.value.size) setSelected(next);
  } catch (err) {
    showFatalError(errText(err));
  }
}

// --- Phase 5c-2：浏览器集成状态与提示条 ----------------------------------
async function loadFirefoxStatus(): Promise<void> {
  try {
    firefoxStatus.value = await invoke<FirefoxStatus>('firefox_extension_status');
  } catch (err) {
    showFatalError(errText(err));
  }
}

async function loadBrowserStatus(): Promise<void> {
  try {
    browserStatus.value = await invoke<BrowserStatus>('browser_integration_status');
    await loadFirefoxStatus();
    const saved = await invoke<Settings>('get_settings');
    const integrationMissing = browserStatus.value === 'NotInstalled';
    const extensionMissingNow = browserStatus.value === 'Ok' && extensionMissing.value;
    browserBannerVisible.value =
      !saved.browser_integration_dismissed && (integrationMissing || extensionMissingNow);
  } catch (err) {
    showFatalError(errText(err));
  }
}

async function onEnableBrowser(): Promise<void> {
  browserActionError.value = '';
  try {
    await invoke('enable_browser_integration');
    browserBannerVisible.value = false;
    await loadBrowserStatus();
  } catch (err) {
    browserActionError.value = errText(err);
  }
}

async function onEnableBrowserFromBanner(): Promise<void> {
  await onEnableBrowser();
}

async function onDisableBrowser(): Promise<void> {
  browserActionError.value = '';
  try {
    await invoke('disable_browser_integration');
    await loadBrowserStatus();
  } catch (err) {
    browserActionError.value = errText(err);
  }
}

async function onDismissBrowserBanner(): Promise<void> {
  try {
    const saved = await invoke<Settings>('get_settings');
    const payload: Settings = { ...saved, browser_integration_dismissed: true };
    await invoke('set_settings', { settings: payload });
    browserBannerVisible.value = false;
  } catch (err) {
    showFatalError(errText(err));
  }
}

async function onInstallFirefox(): Promise<void> {
  firefoxActionMessage.value = '';
  browserActionError.value = '';
  try {
    await invoke('install_firefox_extension');
    firefoxActionMessage.value =
      '请在 Firefox 弹出的窗口中点击添加；建议不勾选“允许在隐私窗口中运行”';
    await loadFirefoxStatus();
  } catch (err) {
    browserActionError.value = errText(err);
  }
}

async function onInstallFirefoxFromBanner(): Promise<void> {
  await onInstallFirefox();
}

async function openOptions(): Promise<void> {
  optionsError.value = '';
  browserActionError.value = '';
  const saved = await invoke<Settings>('get_settings');
  draftSettings.value = { ...saved };
  draftSpeedKb.value = Math.round(saved.global_max_speed / 1024);
  await loadBrowserStatus();
  optionsOpen.value = true;
}
function closeOptions(): void {
  optionsOpen.value = false;
  optionsError.value = '';
}
async function onPickDir(): Promise<void> {
  optionsError.value = '';
  try {
    const path = await invoke<string>('pick_download_dir');
    if (path) draftSettings.value.download_dir = path;
  } catch (err) {
    optionsError.value = errText(err);
  }
}
async function onSaveOptions(): Promise<void> {
  optionsError.value = '';
  const payload: Settings = {
    download_dir: draftSettings.value.download_dir,
    global_max_speed: Math.max(0, Math.round(draftSpeedKb.value)) * 1024,
    max_tasks: draftSettings.value.max_tasks,
    default_connections: draftSettings.value.default_connections,
    max_connections_per_host: draftSettings.value.max_connections_per_host,
    resume_uses_current_connections: draftSettings.value.resume_uses_current_connections,
    autostart: draftSettings.value.autostart,
    notify_on_complete: draftSettings.value.notify_on_complete,
    browser_integration_dismissed: draftSettings.value.browser_integration_dismissed,
  };
  try {
    await invoke('set_settings', { settings: payload });
    globalSpeed.value = payload.global_max_speed;
    closeOptions();
  } catch (err) {
    optionsError.value = errText(err);
  }
}

function stopDetailTimer(): void {
  if (detailTimer !== null) { clearInterval(detailTimer); detailTimer = null; }
}

const DETAIL_STORAGE_KEY = 'idm-rs.detailHeight';
function loadPinnedHeight(): number | null {
  try {
    const raw = localStorage.getItem(DETAIL_STORAGE_KEY);
    if (raw === null) return null;
    const value = Number(raw);
    return Number.isFinite(value) && value > 0 ? value : null;
  } catch {
    return null;
  }
}
function savePinnedHeight(value: number): void {
  try {
    localStorage.setItem(DETAIL_STORAGE_KEY, String(value));
  } catch {
    // localStorage 不可用时静默忽略
  }
}

function chunkKey(c: ChunkView): string {
  return `${c.index}:${c.start}`;
}
function recordChunkSamples(task: TaskDetail): void {
  const now = Date.now();
  const activeKeys = new Set<string>();
  for (const c of task.chunks) {
    const key = chunkKey(c);
    activeKeys.add(key);
    let samples = chunkSamples.get(key) ?? [];
    const prev = samples.length > 0 ? samples[samples.length - 1] : null;
    // 增量为负说明分块被拆分/范围变化，丢弃历史重新计算。
    if (prev !== null && c.downloaded < prev.downloaded) samples = [];
    samples.push({ t: now, downloaded: c.downloaded });
    // 只保留最近约 2 秒的样本。
    const cutoff = now - 2000;
    while (samples.length > 1 && samples[0].t < cutoff) samples.shift();
    chunkSamples.set(key, samples);
  }
  // 清掉不再存在的分块样本。
  for (const key of Array.from(chunkSamples.keys())) {
    if (!activeKeys.has(key)) chunkSamples.delete(key);
  }
  computeChunkSpeeds(task);
}
function computeChunkSpeeds(task: TaskDetail): void {
  chunkSpeeds.clear();
  for (const c of task.chunks) {
    if (c.speed_bps > 0) {
      chunkSpeeds.set(chunkKey(c), c.speed_bps);
      continue;
    }
    const samples = chunkSamples.get(chunkKey(c)) ?? [];
    if (samples.length < 2) {
      chunkSpeeds.set(chunkKey(c), 0);
      continue;
    }
    const first = samples[0];
    const last = samples[samples.length - 1];
    const seconds = (last.t - first.t) / 1000;
    const delta = last.downloaded - first.downloaded;
    chunkSpeeds.set(chunkKey(c), seconds > 0 && delta >= 0 ? delta / seconds : 0);
  }
}
function chunkSpeedText(c: ChunkView): string {
  if (c.state !== 'active') return '—';
  return formatSpeed(chunkSpeeds.get(chunkKey(c)) ?? 0);
}
function clearChunkSamples(): void {
  chunkSamples.clear();
  chunkSpeeds.clear();
}

async function loadDetail(id: number): Promise<void> {
  try {
    const data = await invoke<TaskDetail>('task_detail', { id });
    detail.value = data;
    recordChunkSamples(data);
    await nextTick();
    updateDetailHeight();
  } catch {
    detail.value = null;
  }
}

function updateDetailHeight(): void {
  // 折叠或未选中任务：不设置高度，表格占满。
  if (selectedIds.value.size !== 1 || detailCollapsed.value) {
    detailHeightStyle.value = {};
    return;
  }
  const content = contentEl.value;
  if (content === null) return;
  const available = content.clientHeight;
  // 上限：表格至少保留「表头+5行」，再扣掉分隔线与内容间距。
  const upper = available - HEADER_PLUS_FIVE_ROWS - 5 - 8;
  // 自动高度：标题栏 + 详情元信息/分段条 + 连接表全部行，刚好放下不出滚动条。
  const head = detailHeadEl.value;
  const tableWrap = chunkTableWrapEl.value;
  const headHeight = head?.offsetHeight ?? 0;
  // detail-meta 约 20px、segbar 16px + 8px 下边距、detail-body 上下 padding 16px。
  const fixedHeight = 20 + 16 + 8 + 16;
  const tableHeight = tableWrap?.scrollHeight ?? 0;
  const natural = headHeight + fixedHeight + tableHeight;
  const desired = pinnedDetailHeight ?? natural;
  const bounded = Math.max(MIN_DETAIL_HEIGHT, Math.min(desired, upper));
  detailHeightStyle.value = { height: `${Math.round(bounded)}px` };
}

function startDrag(event: MouseEvent): void {
  const panel = detailPanelEl.value;
  const content = contentEl.value;
  if (panel === null || content === null) return;
  event.preventDefault();
  const startY = event.clientY;
  const startHeight = panel.offsetHeight;
  const upper = content.clientHeight - HEADER_PLUS_FIVE_ROWS - 5 - 8;
  let currentHeight = startHeight;
  const onMove = (moveEvent: MouseEvent): void => {
    const delta = startY - moveEvent.clientY;
    currentHeight = Math.max(MIN_DETAIL_HEIGHT, Math.min(startHeight + delta, upper));
    detailHeightStyle.value = { height: `${Math.round(currentHeight)}px` };
  };
  const onUp = (): void => {
    window.removeEventListener('mousemove', onMove);
    window.removeEventListener('mouseup', onUp);
    if (Number.isFinite(currentHeight) && currentHeight > 0) {
      pinnedDetailHeight = currentHeight;
      savePinnedHeight(currentHeight);
    }
  };
  window.addEventListener('mousemove', onMove);
  window.addEventListener('mouseup', onUp);
}

function onWindowResize(): void {
  void nextTick().then(() => updateDetailHeight());
}

watch([selectedIds, detailCollapsed], () => {
  stopDetailTimer();
  clearChunkSamples();
  detail.value = null;
  detailHeightStyle.value = {};
  if (selectedIds.value.size === 1 && !detailCollapsed.value) {
    pinnedDetailHeight = loadPinnedHeight();
    const id = Array.from(selectedIds.value)[0] as number;
    void loadDetail(id);
    detailTimer = setInterval(() => { void loadDetail(id); }, 500);
  }
});

function segLeft(c: ChunkView): number {
  if (!detail.value?.total_size || detail.value.total_size <= 0) return 0;
  return (c.start / detail.value.total_size) * 100;
}
function segWidth(c: ChunkView): number {
  if (!detail.value?.total_size || detail.value.total_size <= 0) return 0;
  return ((c.end + 1 - c.start) / detail.value.total_size) * 100;
}
function chunkPct(c: ChunkView): number {
  const len = c.end + 1 - c.start;
  if (len <= 0) return 0;
  return Math.max(0, Math.min(100, (c.downloaded / len) * 100));
}
function chunkClass(c: ChunkView): string {
  switch (c.state) {
    case 'done': return 'done';
    case 'retrying': return 'retrying';
    case 'active': return 'active';
    case 'paused': return 'paused';
    case 'queued':
    default: return 'waiting';
  }
}
function chunkStateText(state: string): string {
  switch (state) {
    case 'active': return '下载中';
    case 'done': return '已完成';
    case 'retrying': return '重试中';
    case 'queued': return '排队';
    case 'paused': return '已暂停';
    default: return '等待';
  }
}

function onKeydown(event: KeyboardEvent): void {
  if (event.key === 'Delete' && selectedIds.value.size > 0 && !readOnly.value) {
    const target = event.target as HTMLElement | null;
    if (target && (target.tagName === 'INPUT' || target.tagName === 'TEXTAREA')) return;
    askDeleteSelected();
  }
  if (event.key === 'Escape') {
    contextMenu.value = { ...contextMenu.value, visible: false };
  }
}

onMounted(async () => {
  try {
    readOnly.value = await invoke<boolean>('is_read_only');
    const saved = await invoke<Settings>('get_settings');
    globalSpeed.value = saved.global_max_speed;
    await loadBrowserStatus();
    await refreshTasks();
    unlisten = await listen<string>('tasks-updated', (event) => {
      try {
        const parsed: unknown = JSON.parse(event.payload);
        if (Array.isArray(parsed)) tasks.value = parsed as TaskView[];
      } catch {
        // 忽略非法载荷
      }
    });
    window.addEventListener('keydown', onKeydown);
    window.addEventListener('resize', onWindowResize);
    pinnedDetailHeight = loadPinnedHeight();
  } catch (err) {
    showFatalError(errText(err));
  }
});

onUnmounted(() => {
  if (unlisten) { unlisten(); unlisten = null; }
  stopDetailTimer();
  window.removeEventListener('keydown', onKeydown);
  window.removeEventListener('resize', onWindowResize);
  dismissFatalError();
});
</script>

<style>
:root {
  --bg: #ffffff;
  --fg: #222222;
  --border: #d0d0d0;
  --header-bg: #f0f0f0;
  --row-alt: #f7f7f7;
  --row-hover: #e8f0ff;
  --sel-bg: #cfe0ff;
  --bar-bg: #f5f5f5;
  --track: #e6e6e6;
  --panel-bg: #fafafa;
  --danger: #c00000;
  --err-bg: #f8d7da;
  --err-border: #f5c6cb;
  --warn-bg: #fff3cd;
  --warn-fg: #856404;
  --warn-border: #ffeeba;
  --blue: #2d7ff9;
  --green: #2eb85c;
  --gray: #9aa0a6;
  --red: #e55353;
  --orange: #f0a020;
}
@media (prefers-color-scheme: dark) {
  :root {
    --bg: #1e1e1e;
    --fg: #e0e0e0;
    --border: #3a3a3a;
    --header-bg: #2a2a2a;
    --row-alt: #262626;
    --row-hover: #2c3a4a;
    --sel-bg: #344a66;
    --bar-bg: #262626;
    --track: #3a3a3a;
    --panel-bg: #242424;
    --danger: #ff6b6b;
    --err-bg: #4a2328;
    --err-border: #6b3038;
    --warn-bg: #433a1f;
    --warn-fg: #f0d98a;
    --warn-border: #665828;
  }
}
* { box-sizing: border-box; }
html, body { margin: 0; height: 100%; }
body {
  font-family: "Microsoft YaHei UI", "Segoe UI", sans-serif;
  font-size: 13px;
  color: var(--fg);
  background: var(--bg);
}
.app { display: flex; flex-direction: column; height: 100vh; padding: 10px; gap: 8px; }
h1 { font-size: 16px; margin: 0; }
.banner {
  background: var(--warn-bg); border: 1px solid var(--warn-border); color: var(--warn-fg);
  padding: 8px; border-radius: 4px;
}
.error-banner {
  background: var(--err-bg); border: 1px solid var(--err-border); color: var(--danger);
  padding: 8px; border-radius: 4px; display: flex; align-items: center; justify-content: space-between; gap: 12px;
}
.error-text { color: var(--danger); }
.error-close { border: none; background: transparent; color: var(--danger); font-size: 18px; line-height: 1; cursor: pointer; }
.browser-banner { display: flex; align-items: center; gap: 10px; }
.browser-banner span { flex: 1; }
.browser-action { padding: 3px 10px; }
.browser-status-row { display: flex; align-items: center; gap: 10px; }
.browser-status-row span { flex: 1; }
.toolbar { display: flex; gap: 6px; flex-wrap: wrap; }
button {
  font-family: inherit; font-size: 13px; padding: 4px 10px; cursor: pointer;
  color: var(--fg); background: var(--bar-bg); border: 1px solid var(--border); border-radius: 3px;
}
button:disabled { cursor: not-allowed; opacity: 0.5; }
button .ico { vertical-align: -2px; }
.ico { width: 16px; height: 16px; fill: none; stroke: currentColor; stroke-width: 1.3; stroke-linecap: round; stroke-linejoin: round; }
.main { display: flex; flex: 1; min-height: 0; gap: 8px; }
.sidebar {
  width: 160px; flex-shrink: 0; overflow-y: auto; background: var(--panel-bg);
  border: 1px solid var(--border); border-radius: 4px; padding: 6px;
}
.side-title { font-weight: bold; padding: 4px 6px; color: var(--fg); }
.kind-title { margin-top: 10px; border-top: 1px solid var(--border); padding-top: 10px; }
.side-item {
  display: flex; align-items: center; gap: 6px; width: 100%; border: none; background: transparent;
  text-align: left; padding: 5px 6px; border-radius: 3px;
}
.side-item:hover { background: var(--row-hover); }
.side-item.active { background: var(--sel-bg); }
.side-item .count { margin-left: auto; color: var(--gray); }
.content { flex: 1; min-width: 0; min-height: 0; display: flex; flex-direction: column; gap: 0; }
.table-wrap { flex: 1 1 auto; min-height: 0; overflow: auto; }
.splitter {
  flex: 0 0 5px; height: 5px; margin: 4px 0; cursor: ns-resize;
  background: var(--border); border-radius: 3px;
}
.splitter:hover { background: var(--blue); }
table.tasks { width: 100%; table-layout: fixed; border-collapse: collapse; }
table.tasks th, table.tasks td {
  border: 1px solid var(--border); padding: 4px 6px; text-align: left; height: 28px; overflow: hidden;
}
table.tasks th { background: var(--header-bg); position: sticky; top: 0; }
table.tasks tbody tr:nth-child(even) { background: var(--row-alt); }
table.tasks tbody tr:hover { background: var(--row-hover); }
table.tasks tbody tr.selected { background: var(--sel-bg); }
table.tasks .col-icon { width: 32px; }
table.tasks .col-name { width: auto; }
table.tasks .col-size { width: 82px; }
table.tasks .col-progress { width: 110px; }
table.tasks .col-speed { width: 96px; }
table.tasks .col-eta { width: 92px; }
table.tasks .col-state { width: 72px; }
table.tasks .name { text-overflow: ellipsis; white-space: nowrap; }
table.tasks .num { text-align: right; font-variant-numeric: tabular-nums; white-space: nowrap; }
.empty { text-align: center; color: var(--gray); padding: 16px; }
.pbar { position: relative; width: 100%; height: 15px; background: var(--track); border-radius: 2px; overflow: hidden; }
.pbar-fill { height: 100%; }
.pbar-fill.active { background: var(--blue); }
.pbar-fill.done { background: var(--green); }
.pbar-fill.paused { background: var(--gray); }
.pbar-fill.failed { background: var(--red); }
.pbar-text { position: absolute; inset: 0; text-align: center; line-height: 15px; color: var(--fg); font-variant-numeric: tabular-nums; }
.statusbar { display: flex; gap: 24px; padding: 6px 4px; border-top: 1px solid var(--border); color: var(--fg); }
.detail { flex: 0 0 auto; display: flex; flex-direction: column; border: 1px solid var(--border); border-radius: 4px; overflow: hidden; }
.detail.collapsed { flex-basis: auto; }
.detail-head {
  flex: 0 0 auto;
  display: flex; align-items: center; justify-content: space-between; padding: 5px 8px;
  background: var(--header-bg); cursor: pointer; font-weight: bold;
}
.fold-btn { padding: 2px 8px; }
.detail-body { flex: 1 1 auto; min-height: 0; overflow: hidden; padding: 8px; display: flex; flex-direction: column; }
.chunk-table-wrap { flex: 1 1 auto; min-height: 0; overflow: auto; }
.detail-meta { margin-bottom: 6px; }
.detail-empty { color: var(--gray); padding: 16px; text-align: center; }
.segbar { position: relative; width: 100%; height: 16px; background: var(--track); margin-bottom: 8px; }
.seg { position: absolute; top: 0; bottom: 0; border-right: 1px solid var(--bg); }
.seg-fill { height: 100%; }
.seg-fill.active { background: var(--blue); }
.seg-fill.done { background: var(--green); }
.seg-fill.retrying { background: var(--orange); }
.seg-fill.paused { background: var(--gray); }
.seg-fill.waiting { background: transparent; }
table.chunks { width: 100%; border-collapse: collapse; font-size: 12px; }
table.chunks th, table.chunks td { border: 1px solid var(--border); padding: 2px 6px; text-align: left; }
table.chunks .num { text-align: right; font-variant-numeric: tabular-nums; }
.ctx-menu {
  position: fixed; z-index: 50; min-width: 130px; background: var(--bg);
  border: 1px solid var(--border); border-radius: 4px; padding: 4px; box-shadow: 0 2px 8px rgba(0,0,0,0.2);
  display: flex; flex-direction: column;
}
.ctx-menu button { border: none; background: transparent; text-align: left; }
.ctx-menu button:hover:not(:disabled) { background: var(--row-hover); }
.overlay {
  position: fixed; inset: 0; background: rgba(0,0,0,0.35); display: flex;
  align-items: center; justify-content: center; z-index: 100;
}
.dialog { background: var(--bg); border: 1px solid var(--border); border-radius: 6px; padding: 20px; min-width: 320px; }
.options-dialog { min-width: 420px; }
.dialog h2 { font-size: 16px; margin: 0 0 12px; }
.dialog input[type='text'], .dialog input[type='number'] {
  width: 100%; padding: 6px; color: var(--fg); background: var(--bg); border: 1px solid var(--border); border-radius: 3px;
}
.field { margin-bottom: 10px; }
.field label { display: block; margin-bottom: 4px; }
.hint { display: block; margin-top: 4px; color: var(--gray); font-size: 12px; }
.checkbox { display: flex; align-items: center; gap: 6px; margin: 10px 0; }
.dir-row { display: flex; gap: 6px; }
.dir-row input { flex: 1; }
.dialog-ops { display: flex; justify-content: flex-end; gap: 8px; margin-top: 12px; }
</style>
