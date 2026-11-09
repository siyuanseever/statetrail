/* 演示用：编出来叫 2.1.999（和 Claude Code 进程名的样子一样），跑它后面的命令（fake_claude.py）并等它结束。
 * tmux 的 pane_current_command 取的是前台进程组组长的名字，就是这个程序，所以 statetrail 认得出「这一格是 Claude」。 */
#include <sys/wait.h>
#include <unistd.h>
int main(int argc, char **argv) {
    if (argc < 2) return 2;
    pid_t p = fork();
    if (p == 0) {
        execvp(argv[1], argv + 1);
        _exit(127);
    }
    int s;
    waitpid(p, &s, 0);
    return 0;
}
