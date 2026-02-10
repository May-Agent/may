// src/lib/store.ts
import { create } from "zustand";
import { v4 as uuidv4 } from "uuid";
import type { Session, Message, Project, Attachment } from "@/types";

interface AppState {
  sessions: Session[];
  currentSessionId: string | null;
  projects: Project[];

  // Actions
  createSession: (projectId?: string) => string;
  setCurrentSession: (sessionId: string) => void;
  addMessage: (
    sessionId: string,
    content: string,
    role: "user" | "assistant",
    attachments?: Attachment[],
  ) => void;
  deleteSession: (sessionId: string) => void;
  updateSessionTitle: (sessionId: string, title: string) => void;
  updateSessionModel: (sessionId: string, model: string) => void;
  updateSessionReasoningEffort: (
    sessionId: string,
    effort: "low" | "medium" | "high",
  ) => void;

  createProject: (name: string, description?: string) => void;
  deleteProject: (projectId: string) => void;

  // Settings
  settings: {
    font: string;
    theme: "light" | "dark" | "system";
  };
  updateSettings: (settings: Partial<AppState["settings"]>) => void;

  searchSessions: (query: string) => Session[];
}

// Mock Data
const MOCK_PROJECTS: Project[] = [
  {
    id: "proj-1",
    name: "Personal",
    description: "Personal projects",
    createdAt: new Date().toISOString(),
  },
  {
    id: "proj-2",
    name: "Work",
    description: "Work related stuff",
    createdAt: new Date().toISOString(),
  },
];

const MOCK_SESSIONS: Session[] = [
  {
    id: "sess-1",
    title: "React Components Help",
    lastModified: new Date().toISOString(),
    preview: "How do I create a reusable button component?",
    projectId: "proj-1",
    model: "gpt-4",
    messages: [
      {
        id: "msg-1",
        role: "user",
        content: "How do I create a reusable button component in React?",
        timestamp: new Date(Date.now() - 100000).toISOString(),
      },
      {
        id: "msg-2",
        role: "assistant",
        content:
          'Here is a simple example of a reusable Button component using TypeScript and Tailwind CSS:\n\n```tsx\nimport React from "react";\n\ninterface ButtonProps extends React.ButtonHTMLAttributes<HTMLButtonElement> {\n  variant?: "primary" | "secondary";\n}\n\nexport const Button: React.FC<ButtonProps> = ({ variant = "primary", className, ...props }) => {\n  const baseStyles = "px-4 py-2 rounded font-medium focus:outline-none";\n  const variants = {\n    primary: "bg-blue-500 text-white hover:bg-blue-600",\n    secondary: "bg-gray-200 text-gray-800 hover:bg-gray-300",\n  };\n\n  return (\n    <button \n      className={`${baseStyles} ${variants[variant]} ${className}`}\n      {...props}\n    />\n  );\n};\n```',
        timestamp: new Date(Date.now() - 90000).toISOString(),
      },
    ],
  },
  {
    id: "sess-2",
    title: "Project Planning",
    lastModified: new Date(Date.now() - 86400000).toISOString(),
    preview: "Let's plan the roadmap for Q4.",
    projectId: "proj-2",
    model: "claude-3-opus",
    messages: [
      {
        id: "msg-3",
        role: "user",
        content: "Let's plan the roadmap for Q4.",
        timestamp: new Date(Date.now() - 86400000).toISOString(),
      },
      {
        id: "msg-4",
        role: "assistant",
        content: "Sure! What are the key goals you want to achieve in Q4?",
        timestamp: new Date(Date.now() - 86390000).toISOString(),
      },
    ],
  },
];

export const useStore = create<AppState>((set, get) => ({
  sessions: MOCK_SESSIONS,
  currentSessionId: MOCK_SESSIONS[0].id,
  projects: MOCK_PROJECTS,
  settings: {
    font: "Inter",
    theme: "system",
  },

  updateSettings: (newSettings) => {
    set((state) => ({
      settings: { ...state.settings, ...newSettings },
    }));
  },

  createSession: (projectId) => {
    const newSession: Session = {
      id: uuidv4(),
      title: "New Chat",
      lastModified: new Date().toISOString(),
      preview: "New conversation started",
      messages: [],
      projectId,
      model: "gpt-4",
    };
    set((state) => ({
      sessions: [newSession, ...state.sessions],
      currentSessionId: newSession.id,
    }));
    return newSession.id;
  },

  setCurrentSession: (sessionId) => {
    set({ currentSessionId: sessionId });
  },

  addMessage: (sessionId, content, role, attachments = []) => {
    const newMessage: Message = {
      id: uuidv4(),
      role,
      content,
      timestamp: new Date().toISOString(),
      attachments,
    };

    set((state) => {
      const updatedSessions = state.sessions.map((session) => {
        if (session.id === sessionId) {
          return {
            ...session,
            messages: [...session.messages, newMessage],
            lastModified: new Date().toISOString(),
            preview:
              content.substring(0, 100) + (content.length > 100 ? "..." : ""),
            // Auto-update title for first user message if it's "New Chat"
            title:
              session.messages.length === 0 && role === "user"
                ? content.substring(0, 30)
                : session.title,
          };
        }
        return session;
      });

      // Move updated session to top
      updatedSessions.sort(
        (a, b) =>
          new Date(b.lastModified).getTime() -
          new Date(a.lastModified).getTime(),
      );

      return { sessions: updatedSessions };
    });
  },

  deleteSession: (sessionId) => {
    set((state) => {
      const newSessions = state.sessions.filter((s) => s.id !== sessionId);
      let newCurrentId = state.currentSessionId;
      if (state.currentSessionId === sessionId) {
        newCurrentId = newSessions.length > 0 ? newSessions[0].id : null;
      }
      return { sessions: newSessions, currentSessionId: newCurrentId };
    });
  },

  updateSessionTitle: (sessionId, title) => {
    set((state) => ({
      sessions: state.sessions.map((s) =>
        s.id === sessionId ? { ...s, title } : s,
      ),
    }));
  },

  updateSessionModel: (sessionId, model) => {
    set((state) => ({
      sessions: state.sessions.map((s) =>
        s.id === sessionId ? { ...s, model } : s,
      ),
    }));
  },

  updateSessionReasoningEffort: (sessionId, effort) => {
    set((state) => ({
      sessions: state.sessions.map((s) =>
        s.id === sessionId ? { ...s, reasoningEffort: effort } : s,
      ),
    }));
  },

  createProject: (name, description) => {
    const newProject: Project = {
      id: uuidv4(),
      name,
      description,
      createdAt: new Date().toISOString(),
    };
    set((state) => ({ projects: [...state.projects, newProject] }));
  },

  deleteProject: (projectId) => {
    set((state) => ({
      projects: state.projects.filter((p) => p.id !== projectId),
      // Also maybe unlink sessions or delete them? For now, keep them but remove projectId
      sessions: state.sessions.map((s) =>
        s.projectId === projectId ? { ...s, projectId: undefined } : s,
      ),
    }));
  },

  searchSessions: (query) => {
    const { sessions } = get();
    if (!query) return sessions;
    const lowerQuery = query.toLowerCase();
    return sessions.filter(
      (s) =>
        s.title.toLowerCase().includes(lowerQuery) ||
        s.messages.some((m) => m.content.toLowerCase().includes(lowerQuery)),
    );
  },
}));
