#ifndef RVB_FREESTANDING_ASSERT_H
#define RVB_FREESTANDING_ASSERT_H

#ifdef NDEBUG
#define assert(expression) ((void)0)
#else
void rvb_assertion_failed(void);
#define assert(expression) ((expression) ? (void)0 : rvb_assertion_failed())
#endif

#endif
