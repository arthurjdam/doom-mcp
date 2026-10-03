// doom-mcp shim: the doomgeneric platform layer plus a small C API that the
// Rust side uses to drive the engine and inspect game state.
//
// The engine's own sources are compiled with -Dexit=dmcp_exit so that
// I_Error/I_Quit unwind back here (via longjmp) instead of killing the
// MCP server process.
#undef exit

#include <math.h>
#include <setjmp.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#include "doomgeneric.h"
#include "d_event.h"
#include "d_loop.h"
#include "d_player.h"
#include "doomstat.h"
#include "g_game.h"
#include "i_timer.h"
#include "m_controls.h"
#include "p_local.h"
#include "r_main.h"
#include "r_state.h"

#include "dmcp.h"

extern int showMessages;

// ---------------------------------------------------------------------------
// Platform layer. Time is virtual: it only moves when the engine sleeps, so
// the game is frozen between MCP calls and every step is deterministic.
// ---------------------------------------------------------------------------

static uint32_t clock_ms;

void DG_Init(void) {}
// Called for every rendered frame (including each step of the screen melt),
// so the spectator view sees everything the engine draws.
static void (*frame_callback)(const uint32_t *);

void dmcp_set_frame_callback(void (*cb)(const uint32_t *)) { frame_callback = cb; }

void DG_DrawFrame(void)
{
    if (frame_callback)
        frame_callback(DG_ScreenBuffer);
}
void DG_SleepMs(uint32_t ms) { clock_ms += ms ? ms : 1; }
uint32_t DG_GetTicksMs(void) { return clock_ms; }
int DG_GetKey(int *pressed, unsigned char *key) { return 0; }
void DG_SetWindowTitle(const char *title) {}

// ---------------------------------------------------------------------------
// exit() interception
// ---------------------------------------------------------------------------

static jmp_buf *active_jmp;
static int exited;
static int exit_status;

void dmcp_exit(int status)
{
    exited = 1;
    exit_status = status;
    if (active_jmp)
        longjmp(*active_jmp, 1);
    _Exit(status);
}

int dmcp_exited(void) { return exited; }
int dmcp_exit_status(void) { return exit_status; }

int dmcp_create(int argc, char **argv)
{
    jmp_buf jb;
    if (setjmp(jb))
    {
        active_jmp = NULL;
        return -1;
    }
    active_jmp = &jb;
    doomgeneric_Create(argc, argv);
    active_jmp = NULL;

    // One game tic per engine iteration, never several at once. Without this,
    // tics that pass during a screen melt are caught up in one go afterwards,
    // which shows as a jump in the spectator video.
    singletics = true;

    // Messages are captured by dmcp_take_message() rather than shown on the
    // HUD, and mouse events map 1:1 onto turn units (see dmcp_turn).
    showMessages = 0;
    mouseSensitivity = 5;
    return 0;
}

static char last_message[128];

// Runs the engine until exactly one more game tic has elapsed (or a small
// safety budget of frames is spent). Returns -1 if the engine exited.
int dmcp_run_tic(void)
{
    jmp_buf jb;
    int start, i, now;

    if (exited)
        return -1;
    if (setjmp(jb))
    {
        active_jmp = NULL;
        return -1;
    }
    active_jmp = &jb;
    start = gametic;

    // Move the virtual clock to the start of the next tic first, so the Tick
    // below runs exactly one game tic and draws exactly one frame. Otherwise
    // TryRunTics only notices the new tic part-way through a Tick, returns
    // without running it, and D_Display redraws the unchanged screen: two
    // frames per tic, which doubles the spectator video's length.
    now = I_GetTime();
    while (I_GetTime() == now)
        clock_ms++;

    for (i = 0; i < 16 && gametic == start; i++)
        doomgeneric_Tick();
    active_jmp = NULL;

    // With showMessages off the HUD never consumes player messages, so
    // collect them here before the next pickup overwrites them.
    if (players[consoleplayer].message)
    {
        strncpy(last_message, players[consoleplayer].message, sizeof(last_message) - 1);
        players[consoleplayer].message = NULL;
    }
    return 0;
}

int dmcp_take_message(char *out, int len)
{
    int n;
    if (!last_message[0] || len <= 0)
        return 0;
    strncpy(out, last_message, len - 1);
    out[len - 1] = '\0';
    n = (int)strlen(out);
    last_message[0] = '\0';
    return n;
}

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

void dmcp_key(int down, int key)
{
    event_t ev;
    memset(&ev, 0, sizeof(ev));
    ev.type = down ? ev_keydown : ev_keyup;
    ev.data1 = key;
    // Cheat codes and menu text entry read the typed character from data2.
    ev.data2 = (down && key >= 32 && key < 127) ? key : 0;
    D_PostEvent(&ev);
}

// Positive units turn right. One unit is 8/65536 of a full circle.
void dmcp_turn(int units)
{
    event_t ev;
    memset(&ev, 0, sizeof(ev));
    ev.type = ev_mouse;
    ev.data2 = units;
    D_PostEvent(&ev);
}

void dmcp_keys(dmcp_keys_t *k)
{
    k->up = key_up;
    k->down = key_down;
    k->left = key_left;
    k->right = key_right;
    k->strafeleft = key_strafeleft;
    k->straferight = key_straferight;
    k->fire = key_fire;
    k->use = key_use;
    k->speed = key_speed;
}

int dmcp_new_game(int skill, int episode, int map)
{
    if (exited)
        return -1;
    menuactive = false;
    G_DeferedInitNew((skill_t)skill, episode, map);
    return 0;
}

// ---------------------------------------------------------------------------
// State inspection
// ---------------------------------------------------------------------------

static double bam_to_deg(angle_t a) { return (double)a * (360.0 / 4294967296.0); }

static double norm_deg(double d)
{
    while (d > 180.0) d -= 360.0;
    while (d <= -180.0) d += 360.0;
    return d;
}

// Bearing from the player to (x, y), in degrees: 0 = straight ahead,
// positive = to the right, negative = to the left.
static double bearing_to(mobj_t *pmo, fixed_t x, fixed_t y)
{
    double world = atan2((double)(y - pmo->y), (double)(x - pmo->x)) * 180.0 / M_PI;
    return norm_deg(bam_to_deg(pmo->angle) - world);
}

void dmcp_state(dmcp_state_t *s)
{
    player_t *p = &players[consoleplayer];
    int i;

    memset(s, 0, sizeof(*s));
    s->gamemode = gamemode;
    s->gamestate = gamestate;
    s->menuactive = menuactive;
    s->paused = paused;
    s->demoplayback = demoplayback;
    s->automapactive = automapactive;
    s->episode = gameepisode;
    s->map = gamemap;
    s->skill = gameskill;
    s->gametic = gametic;
    s->leveltime = leveltime;
    s->totalkills = totalkills;
    s->totalitems = totalitems;
    s->totalsecrets = totalsecret;

    if (gamestate != GS_LEVEL || !p->mo)
        return;

    s->in_level = 1;
    s->playerstate = p->playerstate;
    s->health = p->health;
    s->armor = p->armorpoints;
    s->armortype = p->armortype;
    s->readyweapon = p->readyweapon;
    s->pendingweapon = p->pendingweapon;
    for (i = 0; i < NUMWEAPONS && i < 9; i++)
        s->weaponowned[i] = p->weaponowned[i];
    for (i = 0; i < NUMAMMO && i < 4; i++)
    {
        s->ammo[i] = p->ammo[i];
        s->maxammo[i] = p->maxammo[i];
    }
    for (i = 0; i < NUMCARDS && i < 6; i++)
        s->cards[i] = p->cards[i];
    for (i = 0; i < NUMPOWERS && i < 6; i++)
        s->powers[i] = p->powers[i];
    s->backpack = p->backpack;
    s->kills = p->killcount;
    s->items = p->itemcount;
    s->secrets = p->secretcount;
    s->x = p->mo->x / (double)FRACUNIT;
    s->y = p->mo->y / (double)FRACUNIT;
    s->z = p->mo->z / (double)FRACUNIT;
    s->angle = bam_to_deg(p->mo->angle);
    s->damagecount = p->damagecount;
    s->bonuscount = p->bonuscount;
    if (p->attacker && p->attacker != p->mo)
    {
        s->has_attacker = 1;
        s->attacker_type = p->attacker->type;
        s->attacker_bearing = bearing_to(p->mo, p->attacker->x, p->attacker->y);
    }
}

int dmcp_things(dmcp_thing_t *out, int max, double radius)
{
    player_t *p = &players[consoleplayer];
    thinker_t *th;
    int n = 0;

    if (gamestate != GS_LEVEL || !p->mo)
        return 0;

    for (th = thinkercap.next; th != &thinkercap && n < max; th = th->next)
    {
        mobj_t *mo;
        double dx, dy, dist;
        dmcp_thing_t *t;

        if (th->function.acp1 != (actionf_p1)P_MobjThinker)
            continue;
        mo = (mobj_t *)th;
        if (mo == p->mo)
            continue;
        if (!(mo->flags & (MF_COUNTKILL | MF_SPECIAL | MF_MISSILE | MF_SHOOTABLE | MF_SOLID))
            && mo->type != MT_TELEPORTMAN)
            continue;

        dx = (mo->x - p->mo->x) / (double)FRACUNIT;
        dy = (mo->y - p->mo->y) / (double)FRACUNIT;
        dist = sqrt(dx * dx + dy * dy);
        if (dist > radius)
            continue;

        t = &out[n++];
        t->id = (int64_t)(intptr_t)mo;
        t->type = mo->type;
        t->flags = mo->flags;
        t->health = mo->health;
        t->x = mo->x / (double)FRACUNIT;
        t->y = mo->y / (double)FRACUNIT;
        t->z = mo->z / (double)FRACUNIT;
        t->distance = dist;
        t->bearing = bearing_to(p->mo, mo->x, mo->y);
        t->line_of_sight = P_CheckSight(p->mo, mo);
        t->targeting_player = (mo->target == p->mo) && (mo->flags & MF_COUNTKILL) && mo->health > 0;
        t->radius = mo->radius / (double)FRACUNIT;
    }
    return n;
}

int dmcp_num_lines(void) { return (gamestate == GS_LEVEL) ? numlines : 0; }

int dmcp_lines(dmcp_line_t *out, int max)
{
    int i, n = 0;
    if (gamestate != GS_LEVEL)
        return 0;
    for (i = 0; i < numlines && n < max; i++)
    {
        line_t *l = &lines[i];
        dmcp_line_t *o = &out[n++];
        o->x1 = l->v1->x / (double)FRACUNIT;
        o->y1 = l->v1->y / (double)FRACUNIT;
        o->x2 = l->v2->x / (double)FRACUNIT;
        o->y2 = l->v2->y / (double)FRACUNIT;
        o->flags = l->flags;
        o->special = l->special;
        o->two_sided = l->backsector != NULL;
        o->front_sector = (int)(l->frontsector - sectors);
        o->back_sector = l->backsector ? (int)(l->backsector - sectors) : -1;
        o->tag = l->tag;
        o->front_floor = l->frontsector->floorheight / (double)FRACUNIT;
        o->front_ceiling = l->frontsector->ceilingheight / (double)FRACUNIT;
        if (l->backsector)
        {
            o->back_floor = l->backsector->floorheight / (double)FRACUNIT;
            o->back_ceiling = l->backsector->ceilingheight / (double)FRACUNIT;
        }
    }
    return n;
}

int dmcp_num_sectors(void) { return (gamestate == GS_LEVEL) ? numsectors : 0; }

int dmcp_sectors(dmcp_sector_t *out, int max)
{
    int i;
    if (gamestate != GS_LEVEL)
        return 0;
    for (i = 0; i < numsectors && i < max; i++)
    {
        out[i].floor = sectors[i].floorheight / (double)FRACUNIT;
        out[i].ceiling = sectors[i].ceilingheight / (double)FRACUNIT;
        out[i].special = sectors[i].special;
        out[i].tag = sectors[i].tag;
        out[i].moving = sectors[i].specialdata != NULL;
        out[i].floorpic = sectors[i].floorpic;
    }
    return i;
}

// Index of the sector containing map point (x, y).
int dmcp_point_sector(double x, double y)
{
    subsector_t *ss;
    if (gamestate != GS_LEVEL)
        return -1;
    ss = R_PointInSubsector((fixed_t)(x * FRACUNIT), (fixed_t)(y * FRACUNIT));
    return (int)(ss->sector - sectors);
}

// The 320x200 framebuffer, one 0x00RRGGBB pixel per uint32.
const uint32_t *dmcp_framebuffer(void) { return DG_ScreenBuffer; }
