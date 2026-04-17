---
title: Parallel Agents - Zed
description: Run multiple agent threads concurrently using the Threads Sidebar, manage them across projects, and isolate work using Git worktrees.
---

# Parallel Agents

Parallel Agents lets you run multiple agent threads at once, each working independently with its own agent, context window, and conversation history. The Threads Sidebar is where you start, manage, and switch between them.

Open the Threads Sidebar with {#kb multi_workspace::ToggleWorkspaceSidebar}.

> **Note:** From version 0.233.0 onward, the Agent Panel and Threads Sidebar are on the left by default. The Project Panel, Git Panel, and other panels move to the right, keeping the thread list and conversation next to each other. To rearrange panels, right-click any panel icon.

## Threads Sidebar {#threads-sidebar}

The sidebar shows your threads grouped by project. Each project gets its own section with a header. Threads appear below with their title, status indicator, and which agent is running them.

To focus the sidebar without toggling it, use {#kb multi_workspace::FocusWorkspaceSidebar}. To search your threads, press {#kb agents_sidebar::FocusSidebarFilter} while the sidebar is focused.

### Switching Threads {#switching-threads}

Click any thread in the sidebar to switch to it. The Agent Panel updates to show that thread's conversation.

For quick switching without opening the sidebar, use the thread switcher: press {#kb agents_sidebar::ToggleThreadSwitcher} to cycle forward through recent threads, or hold `Shift` while pressing that binding to go backward. This works from both the Agent Panel and the Threads Sidebar.

### Threads History {#threads-history}

Threads History holds all your threads. Toggle it with {#kb agents_sidebar::ToggleThreadHistory} or by clicking the View All Threads icon in the sidebar bottom bar.

To move a thread to the Threads History view, hover over it in the sidebar and click the archive icon that appears. You can also select a thread and press {#kb agent::RemoveSelectedThread}. Running threads cannot be moved to history until they finish.

To restore a thread, open Threads History and click the thread you want to bring back. Zed moves it back to the thread list and opens it in the Agent Panel. If the thread was running in a Git worktree that was removed, Zed restores the worktree automatically.

To permanently delete a thread, open Threads History, hover over the thread, and click the trash icon. This removes the thread's conversation history and cleans up any associated worktree data. Deleted threads cannot be recovered.

You can search your threads in history; search will fuzzy match on thread titles.

### Importing External Agent Threads {#importing-threads}

If you have external agents installed, Zed will detect whether you have existing threads and invite you to import them into Zed. Every time you open Threads History, you should see an icon button in the sidebar bottom bar that allows you to import threads at any time. Clicking on it will open a modal, where you can select the agents whose threads you want to import.

## Running Multiple Threads {#running-multiple-threads}

Start a new thread with {#action agent::NewThread}. Each thread runs independently, so you can send a prompt, open a second thread, and give it a different task while the first continues working.

To start a new thread scoped to the currently selected project in the sidebar, use {#action agents_sidebar::NewThreadInGroup}.

Each thread can use a different agent. Click the new thread menu in the Agent Panel toolbar to choose between Zed Agent and any installed [external agents](./external-agents.md). You might run Zed's built-in agent in one thread and an external agent like Claude Code or Codex in another.

## Multiple Projects {#multiple-projects}

The Threads Sidebar can hold multiple projects at once. Each project gets its own group with its own threads and conversation history.

Within a project, you can add multiple folders from a local or remote project. Use {#action workspace::AddFolderToProject} from the command palette, or select **Add Folder to Project** from the project header menu in the sidebar. Agents can then read and write across all of those folders in a single thread.

## Worktree Isolation {#worktree-isolation}

If two threads might edit the same files, start one in a new Git worktree to give it an isolated checkout.

In the Agent Panel toolbar, click the worktree selector to choose which worktree you want the agent to run in, or create a new one. New worktrees start in detached HEAD state, and Zed will attempt to check out the branch you selected. If that branch is already in use by another worktree, the new worktree stays in detached HEAD.

After the agent finishes, review the diff and merge the changes through your normal Git workflow. If the thread was running in a linked worktree and no other active threads use it, moving the thread to Threads History saves the worktree's Git state and removes it from disk. Restoring the thread from history restores the worktree.

## See Also {#see-also}

- [Agent Panel](./agent-panel.md): Manage individual threads and configure the agent
- [External Agents](./external-agents.md): Use Claude Code, Gemini CLI, and other agents
- [Tools](./tools.md): Built-in tools available in each thread
