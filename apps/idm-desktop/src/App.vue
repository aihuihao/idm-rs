<template>
  <div class="app">
    <h1>idm-rs 下载管理器</h1>

    <div v-if="readOnly" class="banner">
      后台运行器正在运行，界面为只读。请先执行 idm-cli runner stop 后重启本程序。
    </div>

    <div v-if="fatalError" class="error-banner">
      <span>{{ fatalError }}</span>
      <button class="error-close" @click="dismissFatalError">×</button>
    </div>

    <div class="toolbar">
      <button :disabled="readOnly || !urlInput.trim()" @click="onAdd">添加任务</button>
      <button :disabled="readOnly" @click="confirmClearCompleted = true">清除已完成</button>
      <button @click="openOptions">选项</button>
    </div>

    <section class="add-bar">
      <input
        v-model="urlInput"
        :disabled="readOnly"
        type="text"
        placeholder="输入下载网址"
        @keyup.enter="onAdd"
      />
      <div v-if="addError" class="error-text">{{ addError }}</div>
    </section>

    <table class="tasks">
      <colgroup>
        <col class="col-name" />
        <col class="col-size" />
        <col class="col-progress" />
        <col class="col-speed" />
        <col class="col-eta" />
        <col class="col-state" />
        <col class="col-ops" />
      </colgroup>
      <thead>
        <tr>
          <th>文件名</th>
          <th class="num">大小</th>
          <th class="num">进度</th>
          <th class="num">速度</th>
          <th class="num">剩余时间</th>
          <th>状态</th>
          <th>操作</th>
        </tr>
      </thead>
      <tbody>
        <tr v-for="task in tasks" :key="task.id">
          <td class="name" :title="task.file_name || task.url_display">
            {{ task.file_name || task.url_display }}
          </td>
          <td class="num">{{ formatSize(task.total_size) }}</td>
          <td class="num">
            <template v-if="task.total_size && task.total_size > 0">
              <progress :value="percent(task)" :max="100"></progress>
              <span class="pct">{{ percent(task) }}%</span>
            </template>
            <span v-else>—</span>
          </td>
          <td class="num">{{ formatSpeed(task.speed_bps) }}</td>
          <td class="num">{{ formatEta(task) }}</td>
          <td :title="task.state === 'Failed' ? (task.last_error || '') : ''">
            {{ stateText(task) }}
          </td>
          <td class="ops">
            <button
              :disabled="readOnly || !canPause(task)"
              @click="onPause(task.id)"
            >暂停</button>
            <button
              :disabled="readOnly || !canResume(task)"
              @click="onResume(task.id)"
            >继续</button>
            <button :disabled="readOnly" @click="askRemove(task)">删除</button>
            <button
              :disabled="task.state !== 'Completed'"
              @click="onReveal(task.id)"
            >打开文件夹</button>
          </td>
        </tr>
        <tr v-if="tasks.length === 0">
          <td colspan="7" class="empty">暂无任务</td>
        </tr>
      </tbody>
    </table>

    <div v-if="confirmTarget" class="overlay">
      <div class="dialog">
        <p>确定删除任务「{{ confirmTarget.file_name || confirmTarget.url_display }}」吗？</p>
        <label class="checkbox">
          <input v-model="deleteFile" type="checkbox" />
          同时删除已下载的文件
        </label>
        <div v-if="confirmError" class="error-text">{{ confirmError }}</div>
        <div class="dialog-ops">
          <button :disabled="readOnly" @click="confirmTarget = null">取消</button>
          <button :disabled="readOnly" @click="onConfirmRemove">确定</button>
        </div>
      </div>
    </div>

    <div v-if="confirmClearCompleted" class="overlay">
      <div class="dialog">
        <p>确定清除所有已完成的任务记录吗？已下载的文件不会被删除。</p>
        <div v-if="clearError" class="error-text">{{ clearError }}</div>
        <div class="dialog-ops">
          <button :disabled="readOnly" @click="confirmClearCompleted = false">取消</button>
          <button :disabled="readOnly" @click="onClearCompleted">确定</button>
        </div>
      </div>
    </div>

    <div v-if="optionsOpen" class="overlay">
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
          <input
            v-model.number="draftSettings.max_tasks"
            :disabled="readOnly"
            type="number"
            min="1"
            max="16"
          />
        </div>
        <div class="field">
          <label>每任务默认连接数（1~32）</label>
          <input
            v-model.number="draftSettings.default_connections"
            :disabled="readOnly"
            type="number"
            min="1"
            max="32"
          />
        </div>
        <div class="field">
          <label class="checkbox">
            <input v-model="draftSettings.autostart" :disabled="readOnly" type="checkbox" />
            开机自动启动（以托盘方式启动）
          </label>
        </div>
        <div class="field">
          <label class="checkbox">
            <input
              v-model="draftSettings.notify_on_complete"
              :disabled="readOnly"
              type="checkbox"
            />
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
import { onMounted, onUnmounted, ref } from 'vue';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';

interface TaskView {
  id: number;
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
  autostart: boolean;
  notify_on_complete: boolean;
}

const readOnly = ref(false);
const tasks = ref<TaskView[]>([]);
const urlInput = ref('');
const addError = ref('');
const fatalError = ref('');
const confirmTarget = ref<TaskView | null>(null);
const deleteFile = ref(false);
const confirmError = ref('');
const confirmClearCompleted = ref(false);
const clearError = ref('');

const optionsOpen = ref(false);
const draftSettings = ref<Settings>({
  download_dir: '',
  global_max_speed: 0,
  max_tasks: 3,
  default_connections: 8,
  autostart: false,
  notify_on_complete: true,
});
const draftSpeedKb = ref(0);
const optionsError = ref('');

let unlisten: (() => void) | null = null;
let fatalTimer: ReturnType<typeof setTimeout> | null = null;

function dismissFatalError(): void {
  if (fatalTimer !== null) {
    clearTimeout(fatalTimer);
    fatalTimer = null;
  }
  fatalError.value = '';
}

function showFatalError(message: string): void {
  dismissFatalError();
  fatalError.value = message;
  fatalTimer = setTimeout(() => {
    fatalTimer = null;
    fatalError.value = '';
  }, 8000);
}

function isStaleTaskError(message: string): boolean {
  return message.includes('neither active nor queued') || message.includes('不在活动或排队');
}

function errText(err: unknown): string {
  if (typeof err === 'string') return err;
  if (err instanceof Error) return err.message;
  return String(err);
}

function formatSize(bytes: number | null): string {
  if (bytes === null || bytes === undefined) return '未知';
  const units = ['B', 'KB', 'MB', 'GB'];
  let value = bytes;
  let index = 0;
  while (value >= 1024 && index < units.length - 1) {
    value /= 1024;
    index += 1;
  }
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
    case 'Queued':
      return '排队中';
    case 'Downloading':
      return '下载中';
    case 'Paused':
      return task.pause_reason === 'Interrupted' ? '已中断' : '已暂停';
    case 'Completed':
      return '已完成';
    case 'Failed':
      return '失败';
    default:
      return task.state;
  }
}

function canPause(task: TaskView): boolean {
  return task.state === 'Queued' || task.state === 'Downloading';
}

function canResume(task: TaskView): boolean {
  return task.state === 'Paused' || task.state === 'Failed';
}

async function refreshTasks(): Promise<void> {
  try {
    tasks.value = await invoke<TaskView[]>('list_tasks');
  } catch (err) {
    showFatalError(errText(err));
  }
}

async function onAdd(): Promise<void> {
  addError.value = '';
  const url = urlInput.value.trim();
  if (!url) return;
  try {
    await invoke('add_task', { url, connections: null });
    urlInput.value = '';
    await refreshTasks();
  } catch (err) {
    addError.value = errText(err);
  }
}

async function onPause(id: number): Promise<void> {
  try {
    await invoke('pause_task', { id });
  } catch (err) {
    const message = errText(err);
    if (!isStaleTaskError(message)) {
      showFatalError(message);
      return;
    }
  }
  await refreshTasks();
}

async function onResume(id: number): Promise<void> {
  try {
    await invoke('resume_task', { id });
  } catch (err) {
    const message = errText(err);
    if (!isStaleTaskError(message)) {
      showFatalError(message);
      return;
    }
  }
  await refreshTasks();
}

function askRemove(task: TaskView): void {
  confirmTarget.value = task;
  deleteFile.value = false;
  confirmError.value = '';
}

async function onConfirmRemove(): Promise<void> {
  const target = confirmTarget.value;
  if (!target) return;
  confirmError.value = '';
  try {
    await invoke('remove_task', { id: target.id, deleteFile: deleteFile.value });
    confirmTarget.value = null;
    await refreshTasks();
  } catch (err) {
    confirmError.value = errText(err);
  }
}

async function onReveal(id: number): Promise<void> {
  try {
    await invoke('reveal_task', { id });
  } catch (err) {
    showFatalError(errText(err));
  }
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

async function openOptions(): Promise<void> {
  optionsError.value = '';
  const saved = await invoke<Settings>('get_settings');
  draftSettings.value = { ...saved };
  draftSpeedKb.value = Math.round(saved.global_max_speed / 1024);
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
    autostart: draftSettings.value.autostart,
    notify_on_complete: draftSettings.value.notify_on_complete,
  };
  try {
    await invoke('set_settings', { settings: payload });
    closeOptions();
  } catch (err) {
    optionsError.value = errText(err);
  }
}

onMounted(async () => {
  try {
    readOnly.value = await invoke<boolean>('is_read_only');
    await refreshTasks();
    unlisten = await listen<string>('tasks-updated', (event) => {
      try {
        const parsed: unknown = JSON.parse(event.payload);
        if (Array.isArray(parsed)) {
          tasks.value = parsed as TaskView[];
        }
      } catch {
        // 忽略非法载荷，保留上一次的列表
      }
    });
  } catch (err) {
    showFatalError(errText(err));
  }
});

onUnmounted(() => {
  if (unlisten) {
    unlisten();
    unlisten = null;
  }
  dismissFatalError();
});
</script>

<style>
.app {
  font-family: 'Microsoft YaHei', sans-serif;
  max-width: 960px;
  margin: 0 auto;
  padding: 16px;
  color: #222;
}
h1 {
  font-size: 20px;
  margin: 0 0 12px;
}
.banner {
  background: #fff3cd;
  border: 1px solid #ffeeba;
  color: #856404;
  padding: 10px;
  border-radius: 4px;
  margin-bottom: 12px;
}
.error-banner,
.error-text {
  color: #c00;
}
.error-banner {
  background: #f8d7da;
  border: 1px solid #f5c6cb;
  padding: 10px;
  border-radius: 4px;
  margin-bottom: 12px;
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
}
.error-close {
  border: none;
  background: transparent;
  color: #c00;
  font-size: 18px;
  line-height: 1;
  padding: 0 4px;
}
.toolbar {
  display: flex;
  gap: 8px;
  margin-bottom: 12px;
}
.add-bar {
  display: flex;
  flex-wrap: wrap;
  gap: 8px;
  align-items: center;
  margin-bottom: 16px;
}
.add-bar input[type='text'] {
  flex: 1;
  min-width: 240px;
  padding: 6px;
}
.add-bar .error-text {
  flex-basis: 100%;
}
button {
  padding: 5px 12px;
  cursor: pointer;
}
button:disabled {
  cursor: not-allowed;
  opacity: 0.5;
}
table.tasks {
  width: 100%;
  table-layout: fixed;
  border-collapse: collapse;
  font-size: 13px;
}
table.tasks th,
table.tasks td {
  border: 1px solid #ddd;
  padding: 6px;
  text-align: left;
  overflow: hidden;
}
table.tasks th {
  background: #f5f5f5;
}
table.tasks .col-name {
  width: 30%;
}
table.tasks .col-size {
  width: 10%;
}
table.tasks .col-progress {
  width: 14%;
}
table.tasks .col-speed {
  width: 12%;
}
table.tasks .col-eta {
  width: 12%;
}
table.tasks .col-state {
  width: 10%;
}
table.tasks .col-ops {
  width: 210px;
}
table.tasks .name {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
table.tasks .num {
  text-align: right;
  font-variant-numeric: tabular-nums;
  white-space: nowrap;
}
table.tasks .num progress {
  width: 42px;
  vertical-align: middle;
}
table.tasks .ops {
  white-space: nowrap;
  text-overflow: clip;
}
table.tasks .ops button {
  margin-right: 4px;
}
.pct {
  margin-left: 4px;
}
.empty {
  text-align: center;
  color: #888;
  padding: 16px;
}
.overlay {
  position: fixed;
  inset: 0;
  background: rgba(0, 0, 0, 0.35);
  display: flex;
  align-items: center;
  justify-content: center;
}
.dialog {
  background: #fff;
  padding: 20px;
  border-radius: 6px;
  min-width: 320px;
}
.options-dialog {
  min-width: 420px;
}
.options-dialog h2 {
  font-size: 17px;
  margin: 0 0 12px;
}
.field {
  margin-bottom: 10px;
}
.field label {
  display: block;
  margin-bottom: 4px;
}
.field input[type='text'],
.field input[type='number'] {
  width: 100%;
  box-sizing: border-box;
  padding: 6px;
}
.dir-row {
  display: flex;
  gap: 6px;
}
.dir-row input {
  flex: 1;
}
.checkbox {
  display: flex;
  align-items: center;
  gap: 6px;
  margin: 12px 0;
}
.dialog-ops {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
  margin-top: 12px;
}
</style>
