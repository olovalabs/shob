declare module '../vendor/tauri-controls/tauri-controls.js' {
  import type { ComponentProps, ReactNode } from 'react';

  export interface WindowControlsProps extends ComponentProps<'div'> {
    platform?: 'windows' | 'macos' | 'gnome';
    hide?: boolean;
    hideMethod?: 'display' | 'visibility';
    justify?: boolean;
  }

  export interface WindowTitlebarProps extends ComponentProps<'div'> {
    children?: ReactNode;
    controlsOrder?: 'right' | 'left' | 'platform' | 'system';
    windowControlsProps?: WindowControlsProps;
  }

  export const WindowTitlebar: (props: WindowTitlebarProps) => JSX.Element;
}
